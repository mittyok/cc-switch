//! Codex Chat Completions 流式转换模块
//!
//! 将上游 Chat Completions SSE 流式响应转换为 Responses API SSE 格式。
//!
//! Chat Completions 使用简单的 delta chunk 模型，
//! Responses API 使用命名事件 (named events) 的生命周期模型，
//! 两种格式完全不同，需要维护状态机进行转换。

use crate::proxy::sse::{strip_sse_field, take_sse_block};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

// ── Chat Completions data structures ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    _call_type: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

// ── Per-tool-call streaming state ──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ToolState {
    /// Index in the Responses API output array (distinct from the Chat delta index).
    output_index: u32,
    call_id: String,
    name: String,
    accumulated_args: String,
    /// Whether `response.output_item.added` has been emitted for this tool.
    _started: bool,
}

// ── SSE event helper ───────────────────────────────────────────────────────────

/// Format a single named SSE event as `event: …\ndata: …\n\n`.
fn sse_event(event_name: &str, data: &Value) -> Bytes {
    let json_str = serde_json::to_string(data).unwrap_or_default();
    Bytes::from(format!("event: {event_name}\ndata: {json_str}\n\n"))
}

// ── Public API ─────────────────────────────────────────────────────────────────

/// Convert a raw Chat Completions SSE byte stream into a Responses API SSE byte
/// stream.
///
/// The returned stream yields `Bytes` chunks that are fully-formed SSE blocks
/// (`event: …\ndata: …\n\n`).  Each upstream `data: {…}` line is parsed and
/// mapped through a state machine that emits the corresponding Responses API
/// lifecycle events.
pub fn create_responses_sse_stream_from_chat_completions<E: std::error::Error + Send + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        // ── SSE parse buffer ───────────────────────────────────────────────
        let mut buffer = String::new();
        let mut utf8_remainder: Vec<u8> = Vec::new();

        // ── State machine ──────────────────────────────────────────────────
        let mut response_id = String::new();
        let mut model = String::new();
        let output_index: u32 = 0;
        let content_index: u32 = 0;
        let mut accumulated_text = String::new();
        let mut has_sent_created = false;
        let mut has_sent_message_item = false;
        let mut has_sent_content_part = false;
        let mut tool_states: HashMap<usize, ToolState> = HashMap::new();
        let mut finished = false;
        let mut has_sent_completed = false;
        let mut latest_usage: Option<ChatUsage> = None;
        // Item id used for the message output item (stable across events).
        let mut message_item_id = String::new();

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    crate::proxy::sse::append_utf8_safe(
                        &mut buffer,
                        &mut utf8_remainder,
                        &bytes,
                    );

                    while let Some(block) = take_sse_block(&mut buffer) {
                        if block.trim().is_empty() {
                            continue;
                        }

                        for line in block.lines() {
                            let data = match strip_sse_field(line, "data") {
                                Some(d) => d,
                                None => continue,
                            };

                            // ── [DONE] sentinel ────────────────────────────
                            if data.trim() == "[DONE]" {
                                if !finished {
                                    // Close any still-open text / tool items.
                                    for event in finalize_open_items(
                                        &accumulated_text,
                                        has_sent_content_part,
                                        has_sent_message_item,
                                        &message_item_id,
                                        &tool_states,
                                        "completed",
                                    ) {
                                        yield Ok(event);
                                    }
                                    finished = true;
                                }

                                // Emit response.completed
                                if !has_sent_completed {
                                    let completed = build_response_completed(
                                        &response_id,
                                        &model,
                                        "completed",
                                        &accumulated_text,
                                        &message_item_id,
                                        has_sent_message_item,
                                        &tool_states,
                                        &latest_usage,
                                    );
                                    yield Ok(sse_event("response.completed", &completed));
                                    has_sent_completed = true;
                                }
                                continue;
                            }

                            // ── Parse the JSON chunk ───────────────────────
                            let chunk: ChatChunk = match serde_json::from_str(data) {
                                Ok(c) => c,
                                Err(e) => {
                                    log::warn!(
                                        "[Codex/ChatToResponses] Failed to parse chunk: {e}"
                                    );
                                    continue;
                                }
                            };

                            // Capture ids / model from the first chunk.
                            if response_id.is_empty() && !chunk.id.is_empty() {
                                response_id = format!("resp_{}", chunk.id);
                                message_item_id = format!("msg_{}", chunk.id);
                            }
                            if model.is_empty() && !chunk.model.is_empty() {
                                model.clone_from(&chunk.model);
                            }
                            if let Some(u) = chunk.usage {
                                latest_usage = Some(u);
                            }

                            let choice = match chunk.choices.first() {
                                Some(c) => c,
                                None => continue,
                            };

                            // ── response.created (once) ────────────────────
                            if !has_sent_created {
                                let created = json!({
                                    "type": "response.created",
                                    "response": {
                                        "id": &response_id,
                                        "model": &model,
                                        "status": "in_progress",
                                        "output": [],
                                        "usage": {
                                            "input_tokens": 0,
                                            "output_tokens": 0,
                                            "total_tokens": 0
                                        }
                                    }
                                });
                                yield Ok(sse_event("response.created", &created));
                                has_sent_created = true;
                            }

                            // ── Text content deltas ────────────────────────
                            if let Some(text) = &choice.delta.content {
                                if !text.is_empty() {
                                    // Ensure the message output item & content
                                    // part have been announced.
                                    if !has_sent_message_item {
                                        let item_added = json!({
                                            "type": "response.output_item.added",
                                            "output_index": output_index,
                                            "item": {
                                                "type": "message",
                                                "id": &message_item_id,
                                                "role": "assistant",
                                                "content": [],
                                                "status": "in_progress"
                                            }
                                        });
                                        yield Ok(sse_event(
                                            "response.output_item.added",
                                            &item_added,
                                        ));
                                        has_sent_message_item = true;
                                    }
                                    if !has_sent_content_part {
                                        let part_added = json!({
                                            "type": "response.content_part.added",
                                            "item_id": &message_item_id,
                                            "output_index": output_index,
                                            "content_index": content_index,
                                            "part": {
                                                "type": "output_text",
                                                "text": ""
                                            }
                                        });
                                        yield Ok(sse_event(
                                            "response.content_part.added",
                                            &part_added,
                                        ));
                                        has_sent_content_part = true;
                                    }

                                    let delta_event = json!({
                                        "type": "response.output_text.delta",
                                        "item_id": &message_item_id,
                                        "output_index": output_index,
                                        "content_index": content_index,
                                        "delta": text
                                    });
                                    yield Ok(sse_event(
                                        "response.output_text.delta",
                                        &delta_event,
                                    ));
                                    accumulated_text.push_str(text);
                                }
                            }

                            // ── Tool call deltas ───────────────────────────
                            if let Some(tool_calls) = &choice.delta.tool_calls {
                                for tc in tool_calls {
                                    let idx = tc.index;

                                    // First appearance of this tool call —
                                    // id + name arrive together.
                                    if let (Some(id), Some(Some(name))) = (
                                        &tc.id,
                                        tc.function.as_ref().map(|f| &f.name),
                                    ) {
                                        // Allocate an output_index for this
                                        // tool (after the message item if any).
                                        let tool_output_index = if has_sent_message_item {
                                            output_index + 1 + tool_states.len() as u32
                                        } else {
                                            output_index + tool_states.len() as u32
                                        };

                                        let state = ToolState {
                                            output_index: tool_output_index,
                                            call_id: id.clone(),
                                            name: name.clone(),
                                            accumulated_args: String::new(),
                                            _started: true,
                                        };
                                        tool_states.insert(idx, state.clone());

                                        let item_added = json!({
                                            "type": "response.output_item.added",
                                            "output_index": state.output_index,
                                            "item": {
                                                "type": "function_call",
                                                "call_id": &state.call_id,
                                                "name": &state.name,
                                                "arguments": "",
                                                "status": "in_progress"
                                            }
                                        });
                                        yield Ok(sse_event(
                                            "response.output_item.added",
                                            &item_added,
                                        ));
                                    }

                                    // Argument fragment.
                                    if let Some(Some(args)) =
                                        tc.function.as_ref().map(|f| &f.arguments)
                                    {
                                        if !args.is_empty() {
                                            if let Some(state) = tool_states.get_mut(&idx) {
                                                state.accumulated_args.push_str(args);

                                                let delta = json!({
                                                    "type":
                                                        "response.function_call_arguments.delta",
                                                    "item_id": &state.call_id,
                                                    "output_index": state.output_index,
                                                    "delta": args
                                                });
                                                yield Ok(sse_event(
                                                    "response.function_call_arguments.delta",
                                                    &delta,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }

                            // ── finish_reason ──────────────────────────────
                            if let Some(reason) = &choice.finish_reason {
                                if !finished {
                                    let status = map_finish_reason(reason);
                                    for event in finalize_open_items(
                                        &accumulated_text,
                                        has_sent_content_part,
                                        has_sent_message_item,
                                        &message_item_id,
                                        &tool_states,
                                        status,
                                    ) {
                                        yield Ok(event);
                                    }
                                    finished = true;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("[Codex/ChatToResponses] Stream error: {e}");
                    break;
                }
            }
        }

        // ── Stream ended without [DONE] — emit completion if needed ────────
        if !finished && has_sent_created {
            for event in finalize_open_items(
                &accumulated_text,
                has_sent_content_part,
                has_sent_message_item,
                &message_item_id,
                &tool_states,
                "completed",
            ) {
                yield Ok(event);
            }
            finished = true;
        }

        if finished && !has_sent_completed {
            let completed = build_response_completed(
                &response_id,
                &model,
                "completed",
                &accumulated_text,
                &message_item_id,
                has_sent_message_item,
                &tool_states,
                &latest_usage,
            );
            yield Ok(sse_event("response.completed", &completed));
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Map Chat Completions `finish_reason` to a Responses API status string.
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" | "tool_calls" => "completed",
        "length" => "incomplete",
        other => {
            log::warn!("[Codex/ChatToResponses] Unknown finish_reason: {other}");
            "completed"
        }
    }
}

/// Emit all the "done" events for open text content and tool calls.
///
/// Returns a `Vec<Bytes>` so the caller can yield them one at a time inside the
/// `async_stream::stream!` macro (we cannot yield from a helper function).
fn finalize_open_items(
    accumulated_text: &str,
    has_content_part: bool,
    has_message_item: bool,
    message_item_id: &str,
    tool_states: &HashMap<usize, ToolState>,
    status: &str,
) -> Vec<Bytes> {
    let mut events: Vec<Bytes> = Vec::new();

    // ── Close text content ─────────────────────────────────────────────────
    if has_content_part {
        // response.output_text.done
        events.push(sse_event(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "item_id": message_item_id,
                "output_index": 0,
                "content_index": 0,
                "text": accumulated_text
            }),
        ));

        // response.content_part.done
        events.push(sse_event(
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "item_id": message_item_id,
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": accumulated_text
                }
            }),
        ));
    }

    // ── Close message output item ──────────────────────────────────────────
    if has_message_item {
        let mut content_parts = Vec::new();
        if has_content_part {
            content_parts.push(json!({
                "type": "output_text",
                "text": accumulated_text
            }));
        }
        events.push(sse_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "message",
                    "id": message_item_id,
                    "role": "assistant",
                    "content": content_parts,
                    "status": status
                }
            }),
        ));
    }

    // ── Close tool calls ───────────────────────────────────────────────────
    let mut tools: Vec<_> = tool_states.iter().collect();
    tools.sort_by_key(|(idx, _)| *idx);
    for (_idx, state) in tools {
        // response.function_call_arguments.done
        events.push(sse_event(
            "response.function_call_arguments.done",
            &json!({
                "type": "response.function_call_arguments.done",
                "item_id": &state.call_id,
                "output_index": state.output_index,
                "arguments": &state.accumulated_args
            }),
        ));

        // response.output_item.done
        events.push(sse_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": state.output_index,
                "item": {
                    "type": "function_call",
                    "call_id": &state.call_id,
                    "name": &state.name,
                    "arguments": &state.accumulated_args,
                    "status": status
                }
            }),
        ));
    }

    events
}

/// Build the final `response.completed` payload.
fn build_response_completed(
    response_id: &str,
    model: &str,
    status: &str,
    accumulated_text: &str,
    message_item_id: &str,
    has_message_item: bool,
    tool_states: &HashMap<usize, ToolState>,
    usage: &Option<ChatUsage>,
) -> Value {
    let mut output = Vec::new();

    // Message item (if any).
    if has_message_item {
        let mut content = Vec::new();
        if !accumulated_text.is_empty() {
            content.push(json!({
                "type": "output_text",
                "text": accumulated_text
            }));
        }
        output.push(json!({
            "type": "message",
            "id": message_item_id,
            "role": "assistant",
            "content": content,
            "status": status
        }));
    }

    // Tool call items.
    let mut tools: Vec<_> = tool_states.iter().collect();
    tools.sort_by_key(|(idx, _)| *idx);
    for (_idx, state) in tools {
        output.push(json!({
            "type": "function_call",
            "call_id": &state.call_id,
            "name": &state.name,
            "arguments": &state.accumulated_args,
            "status": status
        }));
    }

    let usage_json = match usage {
        Some(u) => json!({
            "input_tokens": u.prompt_tokens,
            "output_tokens": u.completion_tokens,
            "total_tokens": u.total_tokens
        }),
        None => json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0
        }),
    };

    json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "model": model,
            "status": status,
            "output": output,
            "usage": usage_json
        }
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    /// Collect all emitted SSE events as raw strings (one per `Bytes` chunk).
    async fn collect_events(
        stream: impl Stream<Item = Result<Bytes, std::io::Error>>,
    ) -> Vec<String> {
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(Ok(bytes)) = stream.next().await {
            let text = String::from_utf8_lossy(&bytes).to_string();
            events.push(text);
        }
        events
    }

    /// Parse an SSE block into (event_name, data_json).
    fn parse_sse(raw: &str) -> Option<(String, Value)> {
        let mut event_name = String::new();
        let mut data_str = String::new();
        for line in raw.lines() {
            if let Some(e) = strip_sse_field(line, "event") {
                event_name = e.to_string();
            }
            if let Some(d) = strip_sse_field(line, "data") {
                data_str = d.to_string();
            }
        }
        let value: Value = serde_json::from_str(&data_str).ok()?;
        Some((event_name, value))
    }

    /// Build a mock byte stream from a single string of concatenated SSE blocks.
    fn mock_stream(
        input: &str,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
        let data = Bytes::from(input.as_bytes().to_vec());
        stream::iter(vec![Ok(data)])
    }

    // ── Test: simple text streaming ────────────────────────────────────────

    #[tokio::test]
    async fn test_simple_text_streaming() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-abc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-abc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-abc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-abc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        // Expected lifecycle:
        // 1. response.created
        // 2. response.output_item.added (message)
        // 3. response.content_part.added
        // 4. response.output_text.delta ("Hello")
        // 5. response.output_text.delta (" world")
        // 6. response.output_text.done
        // 7. response.content_part.done
        // 8. response.output_item.done (message)
        // 9. response.completed

        let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            event_names,
            vec![
                "response.created",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        // Verify delta contents.
        let (_, delta1) = &parsed[3];
        assert_eq!(delta1["delta"], "Hello");
        let (_, delta2) = &parsed[4];
        assert_eq!(delta2["delta"], " world");

        // Verify output_text.done has accumulated text.
        let (_, text_done) = &parsed[5];
        assert_eq!(text_done["text"], "Hello world");

        // Verify response.completed contains the full text.
        let (_, completed) = parsed.last().unwrap();
        let output = &completed["response"]["output"];
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "Hello world");
    }

    // ── Test: tool call streaming ──────────────────────────────────────────

    #[tokio::test]
    async fn test_tool_call_streaming() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-tc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_123\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-tc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"lo\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-tc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"cation\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-tc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":\\\"NYC\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-tc\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();

        assert!(event_names.contains(&"response.created"));
        assert!(event_names.contains(&"response.output_item.added"));
        assert!(event_names.contains(&"response.function_call_arguments.delta"));
        assert!(event_names.contains(&"response.function_call_arguments.done"));
        assert!(event_names.contains(&"response.completed"));

        // Find the output_item.added for the function_call.
        let tool_added = parsed
            .iter()
            .find(|(n, d)| {
                n == "response.output_item.added" && d["item"]["type"] == "function_call"
            })
            .map(|(_, d)| d)
            .expect("should have function_call output_item.added");
        assert_eq!(tool_added["item"]["name"], "get_weather");
        assert_eq!(tool_added["item"]["call_id"], "call_123");

        // Verify accumulated arguments in done event.
        let args_done = parsed
            .iter()
            .find(|(n, _)| n == "response.function_call_arguments.done")
            .map(|(_, d)| d)
            .expect("should have function_call_arguments.done");
        assert_eq!(args_done["arguments"], "{\"location\":\"NYC\"}");

        // Verify response.completed includes the tool call.
        let (_, completed) = parsed.last().unwrap();
        let tool_output = completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["type"] == "function_call")
            .expect("completed response should include function_call output");
        assert_eq!(tool_output["name"], "get_weather");
        assert_eq!(tool_output["arguments"], "{\"location\":\"NYC\"}");
    }

    // ── Test: [DONE] handling ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_done_signal_emits_completed() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-d\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-d\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        let last_event_name = &parsed.last().unwrap().0;
        assert_eq!(last_event_name, "response.completed");

        let (_, completed) = parsed.last().unwrap();
        assert_eq!(completed["response"]["status"], "completed");
    }

    // ── Test: usage forwarding ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_usage_forwarding_in_completed() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-u\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-u\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        let (_, completed) = parsed.last().unwrap();
        let usage = &completed["response"]["usage"];
        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 15);
    }

    // ── Test: stream ends without [DONE] ───────────────────────────────────

    #[tokio::test]
    async fn test_stream_without_done_still_completes() {
        // Some providers drop [DONE] — we should still emit response.completed.
        let input = concat!(
            "data: {\"id\":\"chatcmpl-nd\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        let event_names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
        assert!(event_names.contains(&"response.completed"));

        let (_, completed) = parsed.last().unwrap();
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "Hi"
        );
    }

    // ── Test: finish_reason "length" maps to "incomplete" ──────────────────

    #[tokio::test]
    async fn test_finish_reason_length_maps_to_incomplete() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-l\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"trunc\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-l\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        // The output_item.done should show "incomplete" status.
        let item_done = parsed
            .iter()
            .find(|(n, _)| n == "response.output_item.done")
            .map(|(_, d)| d)
            .expect("should have output_item.done");
        assert_eq!(item_done["item"]["status"], "incomplete");
    }

    // ── Test: multiple tool calls with different indices ────────────────────

    #[tokio::test]
    async fn test_multiple_tool_calls() {
        let input = concat!(
            "data: {\"id\":\"chatcmpl-mt\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"tool_a\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-mt\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"tool_b\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-mt\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":1}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-mt\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"y\\\":2}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-mt\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        let converted = create_responses_sse_stream_from_chat_completions(mock_stream(input));
        let events = collect_events(converted).await;
        let parsed: Vec<_> = events.iter().filter_map(|e| parse_sse(e)).collect();

        // Should have two output_item.added events for function_call.
        let tool_added_events: Vec<_> = parsed
            .iter()
            .filter(|(n, d)| {
                n == "response.output_item.added" && d["item"]["type"] == "function_call"
            })
            .collect();
        assert_eq!(tool_added_events.len(), 2);

        // Verify both tool calls appear in response.completed.
        let (_, completed) = parsed.last().unwrap();
        let outputs = completed["response"]["output"].as_array().unwrap();
        let tool_outputs: Vec<_> = outputs
            .iter()
            .filter(|o| o["type"] == "function_call")
            .collect();
        assert_eq!(tool_outputs.len(), 2);
        assert_eq!(tool_outputs[0]["name"], "tool_a");
        assert_eq!(tool_outputs[0]["arguments"], "{\"x\":1}");
        assert_eq!(tool_outputs[1]["name"], "tool_b");
        assert_eq!(tool_outputs[1]["arguments"], "{\"y\":2}");
    }
}
