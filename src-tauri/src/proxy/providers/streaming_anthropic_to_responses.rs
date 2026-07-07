//! Anthropic SSE → Responses API SSE 流式转换模块
//!
//! Inverse of `streaming_responses.rs`. Converts Anthropic Messages SSE
//! events to OpenAI Responses API SSE named events.
//!
//! Used when the "codex_use_claude_pipeline" toggle routes Codex traffic
//! through Claude's provider pipeline: the Claude adapter returns Anthropic
//! SSE, and this module converts it to Responses SSE for the Codex CLI.
//!
//! Anthropic SSE lifecycle:
//!   message_start → content_block_start → content_block_delta →
//!   content_block_stop → message_delta → message_stop
//!
//! Responses SSE lifecycle:
//!   response.created → response.output_item.added → response.content_part.added →
//!   response.output_text.delta → response.content_part.done →
//!   response.output_item.done → response.completed

use crate::proxy::sse::{strip_sse_field, take_sse_block};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;

/// State for tracking an open content block in the Anthropic stream.
#[derive(Debug)]
#[allow(dead_code)]
enum BlockKind {
    Text,
    ToolUse { call_id: String, name: String },
    Thinking,
}

/// Create a Responses API SSE stream from an Anthropic Messages SSE stream.
pub fn create_responses_sse_stream_from_anthropic<E: std::error::Error + Send + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder: Vec<u8> = Vec::new();
        let mut response_id = String::new();
        let mut model = String::new();
        let mut output_index: u32 = 0;
        let mut _has_function_call = false;
        // Track open blocks by their Anthropic index
        let mut open_blocks: HashMap<u32, BlockKind> = HashMap::new();
        // Map Anthropic content block index → Responses output_index
        let mut block_to_output: HashMap<u32, u32> = HashMap::new();
        // Track text content_index within current message output item
        let mut text_content_index: u32 = 0;
        // Output index of the current message item (for text content)
        let mut message_output_index: Option<u32> = None;

        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    crate::proxy::sse::append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);

                    while let Some(block) = take_sse_block(&mut buffer) {
                        if block.trim().is_empty() {
                            continue;
                        }

                        let mut event_type: Option<String> = None;
                        let mut data_parts: Vec<String> = Vec::new();

                        for line in block.lines() {
                            if let Some(evt) = strip_sse_field(line, "event") {
                                event_type = Some(evt.trim().to_string());
                            } else if let Some(d) = strip_sse_field(line, "data") {
                                data_parts.push(d.to_string());
                            }
                        }

                        if data_parts.is_empty() {
                            continue;
                        }

                        let data_str = data_parts.join("\n");
                        let event_name = event_type.as_deref().unwrap_or("");

                        let data: Value = match serde_json::from_str(&data_str) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        log::debug!("[Codex←Claude] <<< Anthropic SSE: {event_name}");

                        match event_name {
                            // ================================================
                            // message_start → response.created
                            // ================================================
                            "message_start" => {
                                if let Some(msg) = data.get("message") {
                                    response_id = msg
                                        .get("id")
                                        .and_then(|i| i.as_str())
                                        .map(|s| s.to_string())
                                        .unwrap_or_else(|| {
                                            format!("resp_{}", uuid::Uuid::new_v4().simple())
                                        });
                                    model = msg
                                        .get("model")
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let usage = msg.get("usage").cloned().unwrap_or(json!({}));
                                    let input_tokens =
                                        usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let output_tokens =
                                        usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

                                    let event = json!({
                                        "response": {
                                            "id": response_id,
                                            "object": "response",
                                            "status": "in_progress",
                                            "model": model,
                                            "output": [],
                                            "usage": {
                                                "input_tokens": input_tokens,
                                                "output_tokens": output_tokens,
                                                "total_tokens": input_tokens + output_tokens
                                            }
                                        }
                                    });
                                    let sse = format!(
                                        "event: response.created\ndata: {}\n\n",
                                        serde_json::to_string(&event).unwrap_or_default()
                                    );
                                    yield Ok(Bytes::from(sse));
                                }
                            }

                            // ================================================
                            // content_block_start → output_item.added + content_part.added
                            // ================================================
                            "content_block_start" => {
                                let index = data
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0) as u32;
                                let block_type = data
                                    .pointer("/content_block/type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");

                                match block_type {
                                    "text" => {
                                        // Reuse or create a message output item
                                        let msg_idx = if let Some(idx) = message_output_index {
                                            idx
                                        } else {
                                            let idx = output_index;
                                            output_index += 1;
                                            message_output_index = Some(idx);

                                            // Emit output_item.added for the message
                                            let item_event = json!({
                                                "output_index": idx,
                                                "item": {
                                                    "type": "message",
                                                    "role": "assistant",
                                                    "content": []
                                                }
                                            });
                                            let sse = format!(
                                                "event: response.output_item.added\ndata: {}\n\n",
                                                serde_json::to_string(&item_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                            idx
                                        };

                                        block_to_output.insert(index, msg_idx);
                                        open_blocks.insert(index, BlockKind::Text);

                                        let part_event = json!({
                                            "output_index": msg_idx,
                                            "content_index": text_content_index,
                                            "part": {
                                                "type": "output_text",
                                                "text": ""
                                            }
                                        });
                                        let sse = format!(
                                            "event: response.content_part.added\ndata: {}\n\n",
                                            serde_json::to_string(&part_event).unwrap_or_default()
                                        );
                                        yield Ok(Bytes::from(sse));
                                    }

                                    "tool_use" => {
                                        // Close current message item if any text was open
                                        // (tool calls are separate top-level output items)
                                        if message_output_index.is_some() {
                                            message_output_index = None;
                                            text_content_index = 0;
                                        }

                                        _has_function_call = true;
                                        let call_id = data
                                            .pointer("/content_block/id")
                                            .and_then(|i| i.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = data
                                            .pointer("/content_block/name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("")
                                            .to_string();

                                        let idx = output_index;
                                        output_index += 1;
                                        block_to_output.insert(index, idx);
                                        open_blocks.insert(
                                            index,
                                            BlockKind::ToolUse {
                                                call_id: call_id.clone(),
                                                name: name.clone(),
                                            },
                                        );

                                        let item_event = json!({
                                            "output_index": idx,
                                            "item": {
                                                "type": "function_call",
                                                "call_id": call_id,
                                                "name": name,
                                                "arguments": "",
                                                "status": "in_progress"
                                            }
                                        });
                                        let sse = format!(
                                            "event: response.output_item.added\ndata: {}\n\n",
                                            serde_json::to_string(&item_event).unwrap_or_default()
                                        );
                                        yield Ok(Bytes::from(sse));
                                    }

                                    "thinking" => {
                                        let idx = output_index;
                                        output_index += 1;
                                        block_to_output.insert(index, idx);
                                        open_blocks.insert(index, BlockKind::Thinking);

                                        let item_event = json!({
                                            "output_index": idx,
                                            "item": {
                                                "type": "reasoning",
                                                "summary": []
                                            }
                                        });
                                        let sse = format!(
                                            "event: response.output_item.added\ndata: {}\n\n",
                                            serde_json::to_string(&item_event).unwrap_or_default()
                                        );
                                        yield Ok(Bytes::from(sse));
                                    }

                                    _ => {}
                                }
                            }

                            // ================================================
                            // content_block_delta → type-specific delta events
                            // ================================================
                            "content_block_delta" => {
                                let index = data
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0) as u32;
                                let delta_type = data
                                    .pointer("/delta/type")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                let out_idx = block_to_output
                                    .get(&index)
                                    .copied()
                                    .unwrap_or(0);

                                match delta_type {
                                    "text_delta" => {
                                        if let Some(text) =
                                            data.pointer("/delta/text").and_then(|t| t.as_str())
                                        {
                                            let event = json!({
                                                "output_index": out_idx,
                                                "content_index": text_content_index,
                                                "delta": text
                                            });
                                            let sse = format!(
                                                "event: response.output_text.delta\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                        }
                                    }

                                    "input_json_delta" => {
                                        if let Some(json_str) = data
                                            .pointer("/delta/partial_json")
                                            .and_then(|j| j.as_str())
                                        {
                                            let event = json!({
                                                "output_index": out_idx,
                                                "delta": json_str
                                            });
                                            let sse = format!(
                                                "event: response.function_call_arguments.delta\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                        }
                                    }

                                    "thinking_delta" => {
                                        if let Some(text) = data
                                            .pointer("/delta/thinking")
                                            .and_then(|t| t.as_str())
                                        {
                                            let event = json!({
                                                "output_index": out_idx,
                                                "delta": text
                                            });
                                            let sse = format!(
                                                "event: response.reasoning_summary_text.delta\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                        }
                                    }

                                    _ => {}
                                }
                            }

                            // ================================================
                            // content_block_stop → done events
                            // ================================================
                            "content_block_stop" => {
                                let index = data
                                    .get("index")
                                    .and_then(|i| i.as_u64())
                                    .unwrap_or(0) as u32;
                                let out_idx = block_to_output.get(&index).copied().unwrap_or(0);

                                if let Some(kind) = open_blocks.remove(&index) {
                                    match kind {
                                        BlockKind::Text => {
                                            let event = json!({
                                                "output_index": out_idx,
                                                "content_index": text_content_index
                                            });
                                            let sse = format!(
                                                "event: response.content_part.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                            text_content_index += 1;
                                        }
                                        BlockKind::ToolUse { .. } => {
                                            let event = json!({
                                                "output_index": out_idx
                                            });
                                            let sse = format!(
                                                "event: response.function_call_arguments.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));

                                            let done_event = json!({
                                                "output_index": out_idx
                                            });
                                            let done_sse = format!(
                                                "event: response.output_item.done\ndata: {}\n\n",
                                                serde_json::to_string(&done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(done_sse));
                                        }
                                        BlockKind::Thinking => {
                                            let event = json!({
                                                "output_index": out_idx
                                            });
                                            let sse = format!(
                                                "event: response.reasoning_summary_text.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));
                                        }
                                    }
                                }
                            }

                            // ================================================
                            // message_delta → (accumulate stop_reason / usage)
                            // ================================================
                            "message_delta" => {
                                // Close any open message output item
                                if let Some(msg_idx) = message_output_index.take() {
                                    let event = json!({
                                        "output_index": msg_idx
                                    });
                                    let sse = format!(
                                        "event: response.output_item.done\ndata: {}\n\n",
                                        serde_json::to_string(&event).unwrap_or_default()
                                    );
                                    yield Ok(Bytes::from(sse));
                                }

                                let stop_reason = data
                                    .pointer("/delta/stop_reason")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("end_turn");

                                let (status, incomplete_details) = match stop_reason {
                                    "max_tokens" => (
                                        "incomplete",
                                        Some(json!({"reason": "max_output_tokens"})),
                                    ),
                                    _ => ("completed", None),
                                };

                                let usage = data.get("usage").cloned().unwrap_or(json!({}));
                                let input_tokens =
                                    usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                let output_tokens =
                                    usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

                                let mut completed = json!({
                                    "response": {
                                        "id": response_id,
                                        "object": "response",
                                        "status": status,
                                        "model": model,
                                        "usage": {
                                            "input_tokens": input_tokens,
                                            "output_tokens": output_tokens,
                                            "total_tokens": input_tokens + output_tokens
                                        }
                                    }
                                });

                                if let Some(details) = incomplete_details {
                                    completed["response"]["incomplete_details"] = details;
                                }

                                let sse = format!(
                                    "event: response.completed\ndata: {}\n\n",
                                    serde_json::to_string(&completed).unwrap_or_default()
                                );
                                yield Ok(Bytes::from(sse));
                            }

                            // message_stop — no Responses equivalent needed
                            // (response.completed already emitted on message_delta)
                            "message_stop" | "ping" => {}

                            _ => {
                                log::debug!(
                                    "[Codex←Claude] Skipping unknown Anthropic SSE event: {event_name}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("[Codex←Claude] Anthropic stream error: {e}");
                    let error_event = json!({
                        "error": {
                            "message": format!("Stream error: {e}"),
                            "type": "stream_error",
                            "code": null,
                            "param": null
                        }
                    });
                    let sse = format!(
                        "event: error\ndata: {}\n\n",
                        serde_json::to_string(&error_event).unwrap_or_default()
                    );
                    yield Ok(Bytes::from(sse));
                    break;
                }
            }
        }
    }
}
