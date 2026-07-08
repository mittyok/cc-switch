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
#[derive(Debug, Clone)]
enum BlockKind {
    Text { accumulated: String },
    ToolUse { call_id: String, name: String, accumulated_args: String },
    Thinking { accumulated: String },
}

/// Accumulated output item for building the final `output` array in response.completed.
#[derive(Debug, Clone)]
enum OutputItem {
    Message { content: Vec<Value> },
    FunctionCall { call_id: String, name: String, arguments: String },
    Reasoning { summary: Vec<Value> },
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
        let mut open_blocks: HashMap<u32, BlockKind> = HashMap::new();
        let mut block_to_output: HashMap<u32, u32> = HashMap::new();
        let mut text_content_index: u32 = 0;
        let mut message_output_index: Option<u32> = None;
        let mut event_count: u32 = 0;
        let mut emitted_count: u32 = 0;
        let mut output_items: HashMap<u32, OutputItem> = HashMap::new();

        log::info!("[Codex←Claude] ▶ Starting Anthropic→Responses SSE stream conversion");

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

                        log::debug!("[Codex←Claude] <<< Anthropic SSE #{event_count}: {event_name}");
                        event_count += 1;

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
                                        "type": "response.created",
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
                                    emitted_count += 1;
                                    log::info!(
                                        "[Codex←Claude] >>> Responses SSE: response.created (id={}, model={})",
                                        &response_id, &model,
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
                                                "type": "response.output_item.added",
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
                                        open_blocks.insert(index, BlockKind::Text { accumulated: String::new() });

                                        let part_event = json!({
                                            "type": "response.content_part.added",
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
                                        if let Some(msg_idx) = message_output_index.take() {
                                            let msg_content = match output_items.get(&msg_idx) {
                                                Some(OutputItem::Message { content }) => content.clone(),
                                                _ => vec![],
                                            };
                                            let done_event = json!({
                                                "type": "response.output_item.done",
                                                "output_index": msg_idx,
                                                "item": {
                                                    "type": "message",
                                                    "role": "assistant",
                                                    "content": msg_content,
                                                    "status": "completed"
                                                }
                                            });
                                            let done_sse = format!(
                                                "event: response.output_item.done\ndata: {}\n\n",
                                                serde_json::to_string(&done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(done_sse));
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
                                                accumulated_args: String::new(),
                                            },
                                        );

                                        let item_event = json!({
                                            "type": "response.output_item.added",
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
                                        open_blocks.insert(index, BlockKind::Thinking { accumulated: String::new() });

                                        let item_event = json!({
                                            "type": "response.output_item.added",
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
                                            if let Some(BlockKind::Text { accumulated }) = open_blocks.get_mut(&index) {
                                                accumulated.push_str(text);
                                            }
                                            let event = json!({
                                                "type": "response.output_text.delta",
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
                                            if let Some(BlockKind::ToolUse { accumulated_args, .. }) = open_blocks.get_mut(&index) {
                                                accumulated_args.push_str(json_str);
                                            }
                                            let event = json!({
                                                "type": "response.function_call_arguments.delta",
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
                                            if let Some(BlockKind::Thinking { accumulated }) = open_blocks.get_mut(&index) {
                                                accumulated.push_str(text);
                                            }
                                            let event = json!({
                                                "type": "response.reasoning_summary_text.delta",
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
                                        BlockKind::Text { accumulated } => {
                                            let event = json!({
                                                "type": "response.content_part.done",
                                                "output_index": out_idx,
                                                "content_index": text_content_index,
                                                "part": {
                                                    "type": "output_text",
                                                    "text": accumulated
                                                }
                                            });
                                            let sse = format!(
                                                "event: response.content_part.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));

                                            // Track for final output
                                            let content_part = json!({
                                                "type": "output_text",
                                                "text": accumulated
                                            });
                                            match output_items.entry(out_idx) {
                                                std::collections::hash_map::Entry::Occupied(mut e) => {
                                                    if let OutputItem::Message { content } = e.get_mut() {
                                                        content.push(content_part);
                                                    }
                                                }
                                                std::collections::hash_map::Entry::Vacant(e) => {
                                                    e.insert(OutputItem::Message { content: vec![content_part] });
                                                }
                                            }

                                            text_content_index += 1;
                                        }
                                        BlockKind::ToolUse { call_id, name, accumulated_args } => {
                                            let event = json!({
                                                "type": "response.function_call_arguments.done",
                                                "output_index": out_idx,
                                                "name": name,
                                                "arguments": accumulated_args
                                            });
                                            let sse = format!(
                                                "event: response.function_call_arguments.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));

                                            // Track for final output
                                            output_items.insert(out_idx, OutputItem::FunctionCall {
                                                call_id: call_id.clone(),
                                                name: name.clone(),
                                                arguments: accumulated_args.clone(),
                                            });

                                            let done_event = json!({
                                                "type": "response.output_item.done",
                                                "output_index": out_idx,
                                                "item": {
                                                    "type": "function_call",
                                                    "call_id": call_id,
                                                    "name": name,
                                                    "arguments": accumulated_args,
                                                    "status": "completed"
                                                }
                                            });
                                            let done_sse = format!(
                                                "event: response.output_item.done\ndata: {}\n\n",
                                                serde_json::to_string(&done_event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(done_sse));
                                        }
                                        BlockKind::Thinking { accumulated } => {
                                            let event = json!({
                                                "type": "response.reasoning_summary_text.done",
                                                "output_index": out_idx,
                                                "text": accumulated
                                            });
                                            let sse = format!(
                                                "event: response.reasoning_summary_text.done\ndata: {}\n\n",
                                                serde_json::to_string(&event).unwrap_or_default()
                                            );
                                            yield Ok(Bytes::from(sse));

                                            // Track for final output
                                            let summary_part = json!({
                                                "type": "summary_text",
                                                "text": accumulated
                                            });
                                            match output_items.entry(out_idx) {
                                                std::collections::hash_map::Entry::Occupied(mut e) => {
                                                    if let OutputItem::Reasoning { summary } = e.get_mut() {
                                                        summary.push(summary_part);
                                                    }
                                                }
                                                std::collections::hash_map::Entry::Vacant(e) => {
                                                    e.insert(OutputItem::Reasoning { summary: vec![summary_part] });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // ================================================
                            // message_delta → (accumulate stop_reason / usage)
                            // ================================================
                            "message_delta" => {
                                // Close any open message output item with accumulated content
                                if let Some(msg_idx) = message_output_index.take() {
                                    let msg_content = match output_items.get(&msg_idx) {
                                        Some(OutputItem::Message { content }) => content.clone(),
                                        _ => vec![],
                                    };
                                    let event = json!({
                                        "type": "response.output_item.done",
                                        "output_index": msg_idx,
                                        "item": {
                                            "type": "message",
                                            "role": "assistant",
                                            "content": msg_content,
                                            "status": "completed"
                                        }
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

                                // Build the final output array from accumulated items
                                let mut final_output: Vec<Value> = Vec::new();
                                let mut sorted_keys: Vec<u32> = output_items.keys().copied().collect();
                                sorted_keys.sort();
                                for key in sorted_keys {
                                    if let Some(item) = output_items.get(&key) {
                                        match item {
                                            OutputItem::Message { content } => {
                                                final_output.push(json!({
                                                    "type": "message",
                                                    "role": "assistant",
                                                    "content": content,
                                                    "status": "completed"
                                                }));
                                            }
                                            OutputItem::FunctionCall { call_id, name, arguments } => {
                                                final_output.push(json!({
                                                    "type": "function_call",
                                                    "call_id": call_id,
                                                    "name": name,
                                                    "arguments": arguments,
                                                    "status": "completed"
                                                }));
                                            }
                                            OutputItem::Reasoning { summary } => {
                                                final_output.push(json!({
                                                    "type": "reasoning",
                                                    "summary": summary
                                                }));
                                            }
                                        }
                                    }
                                }

                                let mut completed = json!({
                                    "type": "response.completed",
                                    "response": {
                                        "id": response_id,
                                        "object": "response",
                                        "status": status,
                                        "model": model,
                                        "output": final_output,
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
                                emitted_count += 1;
                                log::info!(
                                    "[Codex←Claude] >>> Responses SSE: response.completed (status={}, anthropic_events={}, responses_events={}, usage={}in/{}out)",
                                    status, event_count, emitted_count, input_tokens, output_tokens,
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
                        "type": "error",
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

        log::info!(
            "[Codex←Claude] ◀ Stream ended: anthropic_events={event_count}, responses_events={emitted_count}, response_id={response_id}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn anthropic_sse_stream(
        events: Vec<&str>,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
        let data: Vec<Result<Bytes, std::io::Error>> = events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_string())))
            .collect();
        stream::iter(data)
    }

    async fn collect_responses_sse(events: Vec<&str>) -> Vec<(String, Value)> {
        let input = anthropic_sse_stream(events);
        let output = create_responses_sse_stream_from_anthropic(input);
        tokio::pin!(output);

        let mut result = Vec::new();
        let mut buffer = String::new();

        while let Some(chunk) = output.next().await {
            match chunk {
                Ok(bytes) => buffer.push_str(&String::from_utf8_lossy(&bytes)),
                Err(e) => panic!("Stream error: {e}"),
            }
        }

        for block in buffer.split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }
            let mut event_name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(evt) = line.strip_prefix("event: ") {
                    event_name = evt.to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = d.to_string();
                }
            }
            if !event_name.is_empty() && !data.is_empty() {
                if let Ok(json) = serde_json::from_str::<Value>(&data) {
                    result.push((event_name, json));
                }
            }
        }
        result
    }

    #[tokio::test]
    async fn test_simple_text_stream() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        let event_names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();

        assert!(event_names.contains(&"response.created"), "Missing response.created, got: {event_names:?}");
        assert!(event_names.contains(&"response.output_item.added"), "Missing response.output_item.added");
        assert!(event_names.contains(&"response.content_part.added"), "Missing response.content_part.added");
        assert!(event_names.contains(&"response.output_text.delta"), "Missing response.output_text.delta");
        assert!(event_names.contains(&"response.content_part.done"), "Missing response.content_part.done");
        assert!(event_names.contains(&"response.output_item.done"), "Missing response.output_item.done");
        assert!(event_names.contains(&"response.completed"), "Missing response.completed, got: {event_names:?}");

        let completed = result.iter().find(|(n, _)| n == "response.completed").unwrap();
        assert_eq!(completed.1["response"]["status"], "completed");
        assert_eq!(completed.1["response"]["id"], "msg_123");
    }

    #[tokio::test]
    async fn test_tool_use_stream() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_456\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        let event_names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();

        assert!(event_names.contains(&"response.completed"), "Missing response.completed for tool_use, got: {event_names:?}");
    }

    #[tokio::test]
    async fn test_thinking_then_text_stream() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_789\",\"model\":\"claude-opus-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think...\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer is 42.\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":30}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        let event_names: Vec<&str> = result.iter().map(|(n, _)| n.as_str()).collect();

        assert!(event_names.contains(&"response.reasoning_summary_text.delta"));
        assert!(event_names.contains(&"response.output_text.delta"));
        assert!(event_names.contains(&"response.completed"), "Missing response.completed, got: {event_names:?}");

        let completed = result.iter().find(|(n, _)| n == "response.completed").unwrap();
        assert_eq!(completed.1["response"]["status"], "completed");
    }

    #[tokio::test]
    async fn test_max_tokens_produces_incomplete() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_max\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":4096}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        let completed = result.iter().find(|(n, _)| n == "response.completed").unwrap();
        assert_eq!(completed.1["response"]["status"], "incomplete");
        assert_eq!(completed.1["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[tokio::test]
    async fn test_response_completed_is_last_event() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_last\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        let last = result.last().expect("should have events");
        assert_eq!(last.0, "response.completed", "response.completed must be last, got: {}", last.0);
    }

    #[tokio::test]
    async fn test_all_events_have_type_field() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_t\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;
        for (event_name, data) in &result {
            assert!(
                data.get("type").is_some(),
                "Event '{event_name}' is missing 'type' field in data payload. Codex CLI requires this for dispatch. Data: {data}"
            );
            assert_eq!(
                data["type"].as_str().unwrap(),
                event_name.as_str(),
                "Event 'type' field must match SSE event name"
            );
        }
    }

    #[tokio::test]
    async fn test_accumulated_data_in_done_events() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_acc\",\"model\":\"claude-sonnet-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            // Text block
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            // Tool use block
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"exec_command\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":25}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;

        // content_part.done must include accumulated text
        let part_done = result.iter().find(|(n, _)| n == "response.content_part.done").unwrap();
        assert_eq!(part_done.1["part"]["text"], "Hello world", "content_part.done must include accumulated text");
        assert_eq!(part_done.1["part"]["type"], "output_text");

        // function_call_arguments.done must include accumulated arguments and name
        let args_done = result.iter().find(|(n, _)| n == "response.function_call_arguments.done").unwrap();
        assert_eq!(args_done.1["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(args_done.1["name"], "exec_command");

        // output_item.done for tool must include full item with status
        let item_done_events: Vec<_> = result.iter().filter(|(n, _)| n == "response.output_item.done").collect();
        let tool_item_done = item_done_events.iter().find(|(_, d)| d["item"]["type"] == "function_call").unwrap();
        assert_eq!(tool_item_done.1["item"]["name"], "exec_command");
        assert_eq!(tool_item_done.1["item"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(tool_item_done.1["item"]["call_id"], "toolu_01");
        assert_eq!(tool_item_done.1["item"]["status"], "completed");

        // output_item.done for message must include content array
        let msg_item_done = item_done_events.iter().find(|(_, d)| d["item"]["type"] == "message").unwrap();
        let content = msg_item_done.1["item"]["content"].as_array().unwrap();
        assert!(!content.is_empty(), "message output_item.done must have content");
        assert_eq!(content[0]["text"], "Hello world");

        // response.completed must include full output array
        let completed = result.iter().find(|(n, _)| n == "response.completed").unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "output should have message + function_call");

        let msg_output = &output[0];
        assert_eq!(msg_output["type"], "message");
        assert_eq!(msg_output["content"][0]["text"], "Hello world");

        let fc_output = &output[1];
        assert_eq!(fc_output["type"], "function_call");
        assert_eq!(fc_output["name"], "exec_command");
        assert_eq!(fc_output["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(fc_output["status"], "completed");
    }

    #[tokio::test]
    async fn test_thinking_accumulated_in_completed() {
        let events = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_think\",\"model\":\"claude-opus-4-6\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Step 1. \"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Step 2.\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer.\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":30}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let result = collect_responses_sse(events).await;

        // reasoning_summary_text.done must include accumulated text
        let reasoning_done = result.iter().find(|(n, _)| n == "response.reasoning_summary_text.done").unwrap();
        assert_eq!(reasoning_done.1["text"], "Step 1. Step 2.");

        // response.completed output must include reasoning and message
        let completed = result.iter().find(|(n, _)| n == "response.completed").unwrap();
        let output = completed.1["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "Step 1. Step 2.");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "Answer.");
    }
}
