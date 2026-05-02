//! Codex Chat Completions 协议转换模块
//!
//! 实现 OpenAI Responses API ↔ Chat Completions API 格式转换。
//!
//! 当 Codex 供应商配置 `wire_api = "chat_completions"` 时，
//! Codex CLI 发出的 Responses API 请求需要转换为 Chat Completions 格式，
//! 上游返回的 Chat Completions 响应需要转换回 Responses API 格式。

use crate::proxy::error::ProxyError;
use serde_json::{json, Value};

/// Strip `<think>…</think>` blocks from text content.
///
/// Many third-party models embed chain-of-thought inside `<think>` tags in the
/// `content` field.  These waste tokens when round-tripped and confuse clients.
fn strip_think_tags(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start..].find("</think>") {
            remaining = &remaining[start + end + "</think>".len()..];
        } else {
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);
    let trimmed = result.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Responses API request → Chat Completions request
// ---------------------------------------------------------------------------

/// Convert an OpenAI Responses API request body into a Chat Completions request
/// body.
///
/// Field mapping:
/// - `model` → passthrough
/// - `instructions` → system message prepended to `messages`
/// - `input` → `messages` (see `convert_input_to_messages`)
/// - `max_output_tokens` → `max_tokens` (or `max_completion_tokens` for o-series)
/// - `temperature`, `top_p`, `stream`, `tool_choice` → passthrough
/// - `reasoning.effort` → `reasoning_effort`
/// - `tools` → re-wrapped with nested `function` object
/// - `store`, `include`, `previous_response_id` → stripped
/// - `prompt_cache_key` → passthrough if present
pub fn responses_request_to_chat_completions(body: Value) -> Result<Value, ProxyError> {
    let mut result = json!({});

    // model — passthrough
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");
    if !model.is_empty() {
        result["model"] = json!(model);
    }

    // ---- messages --------------------------------------------------------

    let mut messages: Vec<Value> = Vec::new();

    // instructions → system message (prepend)
    if let Some(instructions) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    // input → messages
    if let Some(input) = body.get("input").and_then(|v| v.as_array()) {
        let converted = convert_input_to_messages(input)?;
        messages.extend(converted);
    }

    result["messages"] = json!(messages);

    // ---- scalar parameters -----------------------------------------------

    // max_output_tokens → max_tokens / max_completion_tokens
    if let Some(v) = body.get("max_output_tokens") {
        if super::transform::is_openai_o_series(model) {
            result["max_completion_tokens"] = v.clone();
        } else {
            result["max_tokens"] = v.clone();
        }
    }

    if let Some(v) = body.get("temperature") {
        result["temperature"] = v.clone();
    }
    if let Some(v) = body.get("top_p") {
        result["top_p"] = v.clone();
    }

    // stream（不再注入 stream_options，许多第三方 API 不支持该参数）
    if let Some(v) = body.get("stream") {
        result["stream"] = v.clone();
    }

    // reasoning.effort → reasoning_effort（仅 o 系列模型需要）
    if let Some(effort) = body.pointer("/reasoning/effort").and_then(|v| v.as_str()) {
        if super::transform::supports_reasoning_effort(model) {
            result["reasoning_effort"] = json!(effort);
        }
    }

    // ---- tools -----------------------------------------------------------

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let chat_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                // Responses API flat format:
                //   { "type": "function", "name": "...", "description": "...", "parameters": ... }
                // Chat Completions nested format:
                //   { "type": "function", "function": { "name": "...", ... } }
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                let mut func = json!({
                    "name": name,
                });
                if let Some(desc) = t.get("description") {
                    func["description"] = desc.clone();
                }
                // parameters 必须存在，许多第三方 API 不接受缺失的 parameters 字段
                func["parameters"] = t
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                Some(json!({
                    "type": "function",
                    "function": func
                }))
            })
            .collect();

        if !chat_tools.is_empty() {
            result["tools"] = json!(chat_tools);
        }
    }

    // tool_choice — passthrough (mostly compatible between the two APIs)
    if let Some(v) = body.get("tool_choice") {
        result["tool_choice"] = v.clone();
    }

    // Fields explicitly stripped: store, include, previous_response_id,
    // prompt_cache_key, stream_options (non-standard, third-party APIs reject them)

    Ok(result)
}

/// Convert the Responses API `input` array items into Chat Completions
/// `messages`.
///
/// Handles three kinds of items:
/// 1. **Message items** (`role` present) – content array is remapped.
/// 2. **function_call items** – merged into assistant message `tool_calls`.
/// 3. **function_call_output items** – become `role: "tool"` messages.
fn convert_input_to_messages(input: &[Value]) -> Result<Vec<Value>, ProxyError> {
    let mut messages: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < input.len() {
        let item = &input[i];
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match item_type {
            // ----- function_call → assistant tool_calls (merge consecutive) -----
            "function_call" => {
                let mut tool_calls: Vec<Value> = Vec::new();

                while i < input.len() {
                    let cur = &input[i];
                    if cur.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                        break;
                    }
                    let call_id = cur.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = cur.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let arguments = cur
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");

                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }));

                    i += 1;
                }

                messages.push(json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": tool_calls
                }));
                // `i` already advanced past the consecutive block
                continue;
            }

            // ----- function_call_output → tool message -----
            "function_call_output" => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");

                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output
                }));
            }

            // ----- message items (have a `role` field) -----
            _ => {
                let raw_role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                // 保留 developer 角色：OpenAI 兼容 API 中 developer role 比 system
                // 有更高的指令遵循优先级，对 agent 行为（持续使用工具等）至关重要。
                // 不支持 developer 的 provider 通常会自行降级为 system。
                let role = raw_role;

                let chat_content = convert_responses_content_to_chat(item)?;

                messages.push(json!({
                    "role": role,
                    "content": chat_content
                }));
            }
        }

        i += 1;
    }

    Ok(messages)
}

/// Convert Responses API message content items to Chat Completions content.
///
/// If there is a single text item, returns a plain JSON string.
/// Otherwise returns an array of Chat Completions content parts.
fn convert_responses_content_to_chat(msg: &Value) -> Result<Value, ProxyError> {
    let content = match msg.get("content") {
        Some(Value::Array(arr)) => arr,
        Some(Value::String(s)) => return Ok(json!(s)),
        _ => return Ok(Value::Null),
    };

    if content.is_empty() {
        return Ok(Value::Null);
    }

    let mut parts: Vec<Value> = Vec::new();

    for item in content {
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match item_type {
            "input_text" | "output_text" | "text" => {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    let text = if item_type == "output_text" {
                        strip_think_tags(text)
                    } else {
                        text.to_string()
                    };
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                }
            }
            "input_image" => {
                // Responses API: { "type": "input_image", "image_url": "..." }
                // Chat Completions: { "type": "image_url", "image_url": { "url": "..." } }
                if let Some(url) = item.get("image_url").and_then(|v| v.as_str()) {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": url }
                    }));
                }
            }
            _ => {
                // Unknown content type – skip
            }
        }
    }

    // Flatten single text part to plain string for cleaner output
    if parts.len() == 1 {
        if let Some(text) = parts[0].get("text").and_then(|t| t.as_str()) {
            return Ok(json!(text));
        }
    }

    Ok(json!(parts))
}

// ---------------------------------------------------------------------------
// Chat Completions response → Responses API response
// ---------------------------------------------------------------------------

/// Convert a Chat Completions response body into an OpenAI Responses API
/// response body.
pub fn chat_completions_response_to_responses(body: Value) -> Result<Value, ProxyError> {
    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    let message = choice.and_then(|c| c.get("message"));
    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str());

    let mut output: Vec<Value> = Vec::new();

    // reasoning_content → reasoning output item (prepend before message)
    if let Some(reasoning) = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(|v| v.as_str())
    {
        if !reasoning.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "summary": [{
                    "type": "summary_text",
                    "text": reasoning
                }]
            }));
        }
    }

    // tool_calls → function_call output items
    let has_tool_calls = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
        .map(|arr| !arr.is_empty())
        .unwrap_or(false);

    // text content → message output item
    if let Some(content) = message
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
    {
        let content = strip_think_tags(content);
        if !content.is_empty() {
            output.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": content
                }]
            }));
        }
    }

    // tool_calls → individual function_call items
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
    {
        for tc in tool_calls {
            let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("{}");

            output.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments
            }));
        }
    }

    // status & incomplete_details
    let (status, incomplete_details) =
        map_chat_finish_reason_to_responses_status(finish_reason, has_tool_calls);

    // usage
    let usage = build_responses_usage_from_chat(body.get("usage"));

    // id — prefix with "resp_" if not already prefixed
    let raw_id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id = if raw_id.starts_with("resp_") {
        raw_id.to_string()
    } else {
        format!("resp_{}", raw_id)
    };

    let mut result = json!({
        "id": id,
        "object": "response",
        "model": body.get("model").and_then(|m| m.as_str()).unwrap_or(""),
        "output": output,
        "status": status,
        "usage": usage,
    });

    if let Some(details) = incomplete_details {
        result["incomplete_details"] = details;
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Map Chat Completions `finish_reason` to Responses API `status` and optional
/// `incomplete_details`.
fn map_chat_finish_reason_to_responses_status(
    finish_reason: Option<&str>,
    _has_tool_calls: bool,
) -> (&'static str, Option<Value>) {
    match finish_reason {
        Some("stop") => ("completed", None),
        Some("tool_calls") => ("completed", None),
        Some("length") => ("incomplete", Some(json!({"reason": "max_output_tokens"}))),
        Some("content_filter") => ("completed", None),
        // Default / unknown
        _ => ("completed", None),
    }
}

/// Convert Chat Completions usage to Responses API usage format.
fn build_responses_usage_from_chat(usage: Option<&Value>) -> Value {
    let u = match usage {
        Some(v) if v.is_object() && !v.is_null() => v,
        _ => {
            return json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0
            });
        }
    };

    let input_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output_tokens = u
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_tokens = u
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input_tokens + output_tokens);

    let mut result = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens
    });

    // prompt_tokens_details.cached_tokens → input_tokens_details.cached_tokens
    if let Some(cached) = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
    {
        result["input_tokens_details"] = json!({"cached_tokens": cached});
    }

    result
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Request conversion: Responses → Chat Completions -----------------

    #[test]
    fn test_simple_text_request() {
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert_eq!(result["model"], "gpt-4o");
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        // Single text item should be flattened to string
        assert_eq!(messages[0]["content"], "Hello");
    }

    #[test]
    fn test_request_with_instructions() {
        let input = json!({
            "model": "gpt-4o",
            "instructions": "You are a helpful assistant.",
            "input": [
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hi"}]
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a helpful assistant.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Hi");
    }

    #[test]
    fn test_request_with_function_call_and_output() {
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "What is the weather?"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_123",
                    "name": "get_weather",
                    "arguments": "{\"location\":\"NYC\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_123",
                    "output": "Sunny, 72°F"
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);

        // User message
        assert_eq!(messages[0]["role"], "user");

        // function_call → assistant with tool_calls
        assert_eq!(messages[1]["role"], "assistant");
        assert!(messages[1]["content"].is_null());
        let tool_calls = messages[1]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_123");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            "{\"location\":\"NYC\"}"
        );

        // function_call_output → tool message
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_123");
        assert_eq!(messages[2]["content"], "Sunny, 72°F");
    }

    #[test]
    fn test_consecutive_function_calls_merged() {
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_a",
                    "name": "fn_a",
                    "arguments": "{}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_b",
                    "name": "fn_b",
                    "arguments": "{\"x\":1}"
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        let messages = result["messages"].as_array().unwrap();

        // Two consecutive function_calls should be merged into one assistant message
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        let tool_calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0]["id"], "call_a");
        assert_eq!(tool_calls[1]["id"], "call_b");
    }

    #[test]
    fn test_request_with_tools_format_mapping() {
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string"}
                        },
                        "required": ["location"]
                    }
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        // Nested under "function"
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(
            tools[0]["function"]["description"],
            "Get weather for a location"
        );
        assert!(tools[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn test_stream_flag_no_stream_options() {
        // stream_options 不再注入（第三方 API 兼容性）
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "stream": true
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert_eq!(result["stream"], true);
        assert!(result.get("stream_options").is_none());
    }

    #[test]
    fn test_stream_false_no_stream_options() {
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "stream": false
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert_eq!(result["stream"], false);
        assert!(result.get("stream_options").is_none());
    }

    #[test]
    fn test_o_series_uses_max_completion_tokens() {
        let input = json!({
            "model": "o3-mini",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "max_output_tokens": 4096
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert!(result.get("max_tokens").is_none());
        assert_eq!(result["max_completion_tokens"], 4096);
    }

    #[test]
    fn test_non_o_series_uses_max_tokens() {
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "max_output_tokens": 4096
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert_eq!(result["max_tokens"], 4096);
        assert!(result.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_reasoning_effort_passthrough() {
        let input = json!({
            "model": "o3-mini",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "reasoning": {"effort": "high"}
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        assert_eq!(result["reasoning_effort"], "high");
    }

    #[test]
    fn test_image_content_conversion() {
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "What is in this image?"},
                        {"type": "input_image", "image_url": "https://example.com/image.png"}
                    ]
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        let content = messages[0]["content"].as_array().unwrap();

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "What is in this image?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "https://example.com/image.png"
        );
    }

    #[test]
    fn test_stripped_fields() {
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "store": true,
            "include": ["reasoning.encrypted_content"],
            "previous_response_id": "resp_abc"
        });

        let result = responses_request_to_chat_completions(input).unwrap();

        assert!(result.get("store").is_none());
        assert!(result.get("include").is_none());
        assert!(result.get("previous_response_id").is_none());
    }

    #[test]
    fn test_prompt_cache_key_stripped() {
        // prompt_cache_key 不再透传（第三方 API 兼容性）
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "prompt_cache_key": "cache_key_123"
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        assert!(result.get("prompt_cache_key").is_none());
    }

    // ---- Response conversion: Chat Completions → Responses ----------------

    #[test]
    fn test_response_with_text_content() {
        let input = json!({
            "id": "chatcmpl-abc",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 8,
                "total_tokens": 18
            }
        });

        let result = chat_completions_response_to_responses(input).unwrap();

        assert_eq!(result["id"], "resp_chatcmpl-abc");
        assert_eq!(result["model"], "gpt-4o");
        assert_eq!(result["status"], "completed");

        let output = result["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        let content = output[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "Hello! How can I help you?");
    }

    #[test]
    fn test_response_with_tool_calls() {
        let input = json!({
            "id": "chatcmpl-xyz",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_001",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"location\":\"NYC\"}"
                            }
                        },
                        {
                            "id": "call_002",
                            "type": "function",
                            "function": {
                                "name": "get_time",
                                "arguments": "{\"tz\":\"EST\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 15,
                "total_tokens": 35
            }
        });

        let result = chat_completions_response_to_responses(input).unwrap();

        assert_eq!(result["status"], "completed");

        let output = result["output"].as_array().unwrap();
        // No text content (null), so only tool calls
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_001");
        assert_eq!(output[0]["name"], "get_weather");
        assert_eq!(output[0]["arguments"], "{\"location\":\"NYC\"}");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_002");
        assert_eq!(output[1]["name"], "get_time");
    }

    #[test]
    fn test_response_with_reasoning_content() {
        let input = json!({
            "id": "chatcmpl-r1",
            "model": "o3-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "The answer is 42.",
                    "reasoning_content": "Let me think about this step by step..."
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 10,
                "total_tokens": 15
            }
        });

        let result = chat_completions_response_to_responses(input).unwrap();

        let output = result["output"].as_array().unwrap();
        // reasoning should come before message
        assert_eq!(output.len(), 2);

        assert_eq!(output[0]["type"], "reasoning");
        let summary = output[0]["summary"].as_array().unwrap();
        assert_eq!(summary[0]["type"], "summary_text");
        assert_eq!(
            summary[0]["text"],
            "Let me think about this step by step..."
        );

        assert_eq!(output[1]["type"], "message");
    }

    #[test]
    fn test_usage_mapping() {
        let input = json!({
            "id": "chatcmpl-u",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": {
                    "cached_tokens": 30
                }
            }
        });

        let result = chat_completions_response_to_responses(input).unwrap();

        assert_eq!(result["usage"]["input_tokens"], 100);
        assert_eq!(result["usage"]["output_tokens"], 50);
        assert_eq!(result["usage"]["total_tokens"], 150);
        assert_eq!(result["usage"]["input_tokens_details"]["cached_tokens"], 30);
    }

    #[test]
    fn test_finish_reason_stop() {
        let (status, details) = map_chat_finish_reason_to_responses_status(Some("stop"), false);
        assert_eq!(status, "completed");
        assert!(details.is_none());
    }

    #[test]
    fn test_finish_reason_tool_calls() {
        let (status, details) =
            map_chat_finish_reason_to_responses_status(Some("tool_calls"), true);
        assert_eq!(status, "completed");
        assert!(details.is_none());
    }

    #[test]
    fn test_finish_reason_length() {
        let (status, details) = map_chat_finish_reason_to_responses_status(Some("length"), false);
        assert_eq!(status, "incomplete");
        let details = details.unwrap();
        assert_eq!(details["reason"], "max_output_tokens");
    }

    #[test]
    fn test_finish_reason_content_filter() {
        let (status, details) =
            map_chat_finish_reason_to_responses_status(Some("content_filter"), false);
        assert_eq!(status, "completed");
        assert!(details.is_none());
    }

    #[test]
    fn test_finish_reason_none() {
        let (status, details) = map_chat_finish_reason_to_responses_status(None, false);
        assert_eq!(status, "completed");
        assert!(details.is_none());
    }

    #[test]
    fn test_usage_mapping_no_usage() {
        let usage = build_responses_usage_from_chat(None);
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["output_tokens"], 0);
        assert_eq!(usage["total_tokens"], 0);
    }

    #[test]
    fn test_usage_mapping_partial() {
        let chat_usage = json!({"prompt_tokens": 42});
        let usage = build_responses_usage_from_chat(Some(&chat_usage));
        assert_eq!(usage["input_tokens"], 42);
        assert_eq!(usage["output_tokens"], 0);
        assert_eq!(usage["total_tokens"], 42);
    }

    #[test]
    fn test_response_id_already_prefixed() {
        let input = json!({
            "id": "resp_existing",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        });

        let result = chat_completions_response_to_responses(input).unwrap();
        assert_eq!(result["id"], "resp_existing");
    }

    #[test]
    fn test_string_content_in_input() {
        // Some clients may send content as a plain string instead of array
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "role": "user",
                    "content": "Hello there"
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"], "Hello there");
    }

    #[test]
    fn test_developer_role_preserved() {
        // developer 角色应保留（比 system 有更高的指令遵循优先级）
        let input = json!({
            "model": "gpt-4o",
            "input": [
                {
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "You are a helpful assistant"}]
                },
                {
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Hello"}]
                }
            ]
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "developer");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn test_tool_choice_passthrough() {
        let input = json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "hi"}],
            "tool_choice": "required"
        });

        let result = responses_request_to_chat_completions(input).unwrap();
        assert_eq!(result["tool_choice"], "required");
    }

    #[test]
    fn test_response_with_text_and_tool_calls() {
        // Some models return both content and tool_calls
        let input = json!({
            "id": "chatcmpl-both",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Let me check that for you.",
                    "tool_calls": [{
                        "id": "call_t1",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"q\":\"weather\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        });

        let result = chat_completions_response_to_responses(input).unwrap();
        let output = result["output"].as_array().unwrap();

        // Should have both message and function_call
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_t1");
    }
}
