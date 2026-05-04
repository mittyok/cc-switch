//! 请求处理器
//!
//! 处理各种API端点的HTTP请求
//!
//! 重构后的结构：
//! - 通用逻辑提取到 `handler_context` 和 `response_processor` 模块
//! - 各 handler 只保留独特的业务逻辑
//! - Claude 的格式转换逻辑保留在此文件（用于 OpenRouter 旧接口回退）

use super::{
    error_mapper::{get_error_message, map_proxy_error_to_status},
    handler_config::{
        CLAUDE_PARSER_CONFIG, CODEX_PARSER_CONFIG, GEMINI_PARSER_CONFIG, OPENAI_PARSER_CONFIG,
    },
    handler_context::RequestContext,
    providers::{
        self, get_adapter, get_claude_api_format, streaming::create_anthropic_sse_stream,
        streaming_codex_chat::create_responses_sse_stream_from_chat_completions,
        streaming_gemini::create_anthropic_sse_stream_from_gemini,
        streaming_responses::create_anthropic_sse_stream_from_responses, transform,
        transform_codex_chat, transform_gemini, transform_responses,
    },
    response_processor::{
        create_logged_passthrough_stream, process_response, read_decoded_body,
        strip_entity_headers_for_rebuilt_body, strip_hop_by_hop_response_headers,
        SseUsageCollector,
    },
    server::ProxyState,
    sse::{strip_sse_field, take_sse_block},
    types::*,
    usage::parser::TokenUsage,
    ProxyError,
};
use crate::app_config::AppType;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::{json, Value};

// ============================================================================
// 通用请求归一化
// ============================================================================

/// 归一化 Responses API 请求体，确保第三方 API 兼容性。
///
/// 1. 将 input 中 developer/system 角色的消息提取出来，合并到 instructions 字段
///    （Responses API 的 input 只允许 user/assistant 角色，系统指令应放在 instructions）
/// 2. 确保每个 tool 定义都有 parameters 字段（缺少时补充空对象）
fn normalize_responses_body(mut body: Value) -> Value {
    // 1. 提取 developer/system 消息 → 合并到 instructions
    if let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) {
        let mut extracted_texts: Vec<String> = Vec::new();

        input.retain(|item| {
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "developer" || role == "system" {
                // 提取文本内容
                if let Some(content) = item.get("content") {
                    if let Some(text) = content.as_str() {
                        if !text.is_empty() {
                            extracted_texts.push(text.to_string());
                        }
                    } else if let Some(arr) = content.as_array() {
                        for part in arr {
                            if let Some(text) = part
                                .get("text")
                                .and_then(|t| t.as_str())
                                .filter(|t| !t.is_empty())
                            {
                                extracted_texts.push(text.to_string());
                            }
                        }
                    }
                }
                false // 从 input 中移除
            } else {
                true // 保留
            }
        });

        if !extracted_texts.is_empty() {
            // 合并到已有的 instructions 字段
            let existing = body
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let merged = if existing.is_empty() {
                extracted_texts.join("\n")
            } else {
                format!("{}\n{}", existing, extracted_texts.join("\n"))
            };
            body["instructions"] = json!(merged);
        }
    }

    // 2. 确保 tools 里每个 function 都有 parameters
    if let Some(tools) = body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        for tool in tools.iter_mut() {
            if tool.get("type").and_then(|t| t.as_str()) == Some("function")
                && tool.get("parameters").is_none()
            {
                tool["parameters"] = json!({"type": "object", "properties": {}});
            }
        }
    }

    // 3. 移除第三方 API 不支持的字段
    for key in &["store", "include", "previous_response_id"] {
        if let Some(obj) = body.as_object_mut() {
            obj.remove(*key);
        }
    }

    body
}

/// 将 Chat Completions 请求体中的 system 消息合并到第一条 user 消息。
///
/// 某些第三方 API（如京东云）不支持 `system` 角色，会返回
/// `invalid message role: system` 错误。将 system 内容前置到第一条 user 消息中
/// 可以确保最大兼容性。
fn merge_system_into_user(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };

    // 收集所有 system 消息内容
    let mut system_parts: Vec<String> = Vec::new();
    messages.retain(|msg| {
        if msg.get("role").and_then(|r| r.as_str()) == Some("system") {
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    system_parts.push(text.to_string());
                }
            }
            false
        } else {
            true
        }
    });

    if system_parts.is_empty() {
        return;
    }

    let system_text = system_parts.join("\n");

    // 找到第一条 user 消息，将 system 内容前置
    if let Some(first_user) = messages
        .iter_mut()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        if let Some(content) = first_user.get("content").and_then(|c| c.as_str()) {
            first_user["content"] = json!(format!(
                "[System Instructions]\n{}\n\n{}",
                system_text, content
            ));
        } else {
            // content 是数组格式，在开头插入一个 text 块
            if let Some(arr) = first_user.get_mut("content").and_then(|c| c.as_array_mut()) {
                arr.insert(0, json!({"type": "text", "text": format!("[System Instructions]\n{}", system_text)}));
            }
        }
    } else {
        // 没有 user 消息，创建一条
        messages.insert(0, json!({"role": "user", "content": system_text}));
    }
}

// ============================================================================
// 健康检查和状态查询（简单端点）
// ============================================================================

/// 健康检查
pub async fn health_check() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "healthy",
            "timestamp": chrono::Utc::now().to_rfc3339(),
        })),
    )
}

/// 获取服务状态
pub async fn get_status(State(state): State<ProxyState>) -> Result<Json<ProxyStatus>, ProxyError> {
    let status = state.status.read().await.clone();
    Ok(Json(status))
}

/// `/v1/models` — Returns a synthetic models list with a capped `context_window`.
///
/// Codex Desktop uses `context_window * effective_context_window_percent / 100`
/// to calculate `auto_compact_limit`.  By advertising a smaller context window
/// for third-party providers (like MiniMax) that degrade before their nominal
/// limit, we cause Codex to trigger auto-compaction early enough to keep the
/// model working reliably.
///
/// The context window is capped at 100 000 tokens → auto_compact ≈ 90 000.
pub async fn handle_models(State(state): State<ProxyState>) -> (StatusCode, Json<Value>) {
    // Try to determine the model name from the active Codex provider
    let model_name = {
        let current = state.current_providers.read().await;
        current
            .get("codex")
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "gpt-4o".to_string())
    };

    const CONTEXT_WINDOW: u64 = 100_000;

    let model = json!({
        "id": model_name,
        "object": "model",
        "created": 1_700_000_000u64,
        "owned_by": "cc-switch",
        "context_window": CONTEXT_WINDOW,
        "effective_context_window_percent": 90,
        "auto_compact_token_limit": (CONTEXT_WINDOW as f64 * 0.9) as u64,
    });

    log::debug!(
        "[Models] 返回模型列表: {} (context_window={})",
        model_name,
        CONTEXT_WINDOW
    );

    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": [model]
        })),
    )
}

// ============================================================================
// Claude API 处理器（包含格式转换逻辑）
// ============================================================================

/// 处理 /v1/messages 请求（Claude API）
///
/// Claude 处理器包含独特的格式转换逻辑：
/// - 过去用于 OpenRouter 的 OpenAI Chat Completions 兼容接口（Anthropic ↔ OpenAI 转换）
/// - 现在 OpenRouter 已推出 Claude Code 兼容接口，默认不再启用该转换（逻辑保留以备回退）
pub async fn handle_messages(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, body) = request.into_parts();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let mut ctx =
        RequestContext::new(&state, &body, &headers, AppType::Claude, "Claude", "claude").await?;

    let endpoint = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or(uri.path());

    let is_stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // 转发请求
    let forwarder = ctx.create_forwarder(&state);
    let result = match forwarder
        .forward_with_retry(
            &AppType::Claude,
            endpoint,
            body.clone(),
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    ctx.provider = result.provider;
    let api_format = result
        .claude_api_format
        .as_deref()
        .unwrap_or_else(|| get_claude_api_format(&ctx.provider))
        .to_string();
    let response = result.response;

    // 检查是否需要格式转换（OpenRouter 等中转服务）
    let adapter = get_adapter(&AppType::Claude);
    let needs_transform = adapter.needs_transform(&ctx.provider);

    // Claude 特有：格式转换处理
    if needs_transform {
        return handle_claude_transform(response, &ctx, &state, &body, is_stream, &api_format)
            .await;
    }

    // 通用响应处理（透传模式）
    process_response(response, &ctx, &state, &CLAUDE_PARSER_CONFIG).await
}

/// Claude 格式转换处理（独有逻辑）
///
/// 支持 OpenAI Chat Completions 和 Responses API 两种格式的转换
async fn handle_claude_transform(
    response: super::hyper_client::ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    original_body: &Value,
    is_stream: bool,
    api_format: &str,
) -> Result<axum::response::Response, ProxyError> {
    let status = response.status();
    let is_codex_oauth = ctx
        .provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.as_deref())
        == Some("codex_oauth");
    // Codex OAuth 会把 openai_responses 响应强制升级为 SSE，即使客户端发的是 stream:false。
    // should_use_claude_transform_streaming 默认会把这个组合路由到流式转换器——虽然能避免
    // JSON parse 报 422，但会让非流客户端收到 text/event-stream，违反 Anthropic 非流语义。
    // 这里为这个特定组合打开 override：把上游 SSE 聚合成 Anthropic JSON 回给客户端，其它
    // 场景（任意上游 is_sse、非 Codex OAuth 等）仍沿用原有流式兜底。
    let aggregate_codex_oauth_responses_sse =
        !is_stream && is_codex_oauth && api_format == "openai_responses";
    let use_streaming = if aggregate_codex_oauth_responses_sse {
        false
    } else {
        should_use_claude_transform_streaming(
            is_stream,
            response.is_sse(),
            api_format,
            is_codex_oauth,
        )
    };
    let tool_schema_hints = transform_gemini::extract_anthropic_tool_schema_hints(original_body);
    let tool_schema_hints = (!tool_schema_hints.is_empty()).then_some(tool_schema_hints);

    if use_streaming {
        // 根据 api_format 选择流式转换器
        let stream = response.bytes_stream();
        let sse_stream: Box<
            dyn futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + Unpin,
        > = if api_format == "openai_responses" {
            Box::new(Box::pin(create_anthropic_sse_stream_from_responses(stream)))
        } else if api_format == "gemini_native" {
            Box::new(Box::pin(create_anthropic_sse_stream_from_gemini(
                stream,
                Some(state.gemini_shadow.clone()),
                Some(ctx.provider.id.clone()),
                Some(ctx.session_id.clone()),
                tool_schema_hints.clone(),
            )))
        } else {
            Box::new(Box::pin(create_anthropic_sse_stream(stream)))
        };

        // 创建使用量收集器
        let usage_collector = {
            let state = state.clone();
            let provider_id = ctx.provider.id.clone();
            let model = ctx.request_model.clone();
            let status_code = status.as_u16();
            let start_time = ctx.start_time;

            SseUsageCollector::new(start_time, move |events, first_token_ms| {
                if let Some(usage) = TokenUsage::from_claude_stream_events(&events) {
                    let latency_ms = start_time.elapsed().as_millis() as u64;
                    let state = state.clone();
                    let provider_id = provider_id.clone();
                    let model = model.clone();

                    tokio::spawn(async move {
                        log_usage(
                            &state,
                            &provider_id,
                            "claude",
                            &model,
                            &model,
                            usage,
                            latency_ms,
                            first_token_ms,
                            true,
                            status_code,
                        )
                        .await;
                    });
                } else {
                    log::debug!("[Claude] OpenRouter 流式响应缺少 usage 统计，跳过消费记录");
                }
            })
        };

        // 获取流式超时配置
        let timeout_config = ctx.streaming_timeout_config();

        let logged_stream = create_logged_passthrough_stream(
            sse_stream,
            "Claude/OpenRouter",
            Some(usage_collector),
            timeout_config,
        );

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "Content-Type",
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            "Cache-Control",
            axum::http::HeaderValue::from_static("no-cache"),
        );

        let body = axum::body::Body::from_stream(logged_stream);
        return Ok((headers, body).into_response());
    }

    // 非流式响应转换 (OpenAI/Responses → Anthropic)
    let body_timeout =
        if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
            std::time::Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
        } else {
            std::time::Duration::ZERO
        };
    let (mut response_headers, _status, body_bytes) =
        read_decoded_body(response, ctx.tag, body_timeout).await?;

    let body_str = String::from_utf8_lossy(&body_bytes);

    let upstream_response: Value = if aggregate_codex_oauth_responses_sse {
        responses_sse_to_response_value(&body_str)?
    } else {
        serde_json::from_slice(&body_bytes).map_err(|e| {
            log::error!("[Claude] 解析上游响应失败: {e}, body: {body_str}");
            ProxyError::TransformError(format!("Failed to parse upstream response: {e}"))
        })?
    };

    // 根据 api_format 选择非流式转换器
    let anthropic_response = if api_format == "openai_responses" {
        transform_responses::responses_to_anthropic(upstream_response)
    } else if api_format == "gemini_native" {
        transform_gemini::gemini_to_anthropic_with_shadow_and_hints(
            upstream_response,
            Some(state.gemini_shadow.as_ref()),
            Some(&ctx.provider.id),
            Some(&ctx.session_id),
            tool_schema_hints.as_ref(),
        )
    } else {
        transform::openai_to_anthropic(upstream_response)
    }
    .map_err(|e| {
        log::error!("[Claude] 转换响应失败: {e}");
        e
    })?;

    // 记录使用量
    if let Some(usage) = TokenUsage::from_claude_response(&anthropic_response) {
        let model = anthropic_response
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown");
        let latency_ms = ctx.latency_ms();

        let request_model = ctx.request_model.clone();
        tokio::spawn({
            let state = state.clone();
            let provider_id = ctx.provider.id.clone();
            let model = model.to_string();
            async move {
                log_usage(
                    &state,
                    &provider_id,
                    "claude",
                    &model,
                    &request_model,
                    usage,
                    latency_ms,
                    None,
                    false,
                    status.as_u16(),
                )
                .await;
            }
        });
    }

    // 构建响应
    let mut builder = axum::response::Response::builder().status(status);
    strip_entity_headers_for_rebuilt_body(&mut response_headers);
    strip_hop_by_hop_response_headers(&mut response_headers);

    for (key, value) in response_headers.iter() {
        builder = builder.header(key, value);
    }

    builder = builder.header("content-type", "application/json");

    let response_body = serde_json::to_vec(&anthropic_response).map_err(|e| {
        log::error!("[Claude] 序列化响应失败: {e}");
        ProxyError::TransformError(format!("Failed to serialize response: {e}"))
    })?;

    let body = axum::body::Body::from(response_body);
    builder.body(body).map_err(|e| {
        log::error!("[Claude] 构建响应失败: {e}");
        ProxyError::Internal(format!("Failed to build response: {e}"))
    })
}

fn endpoint_with_query(uri: &axum::http::Uri, endpoint: &str) -> String {
    match uri.query() {
        Some(query) => format!("{endpoint}?{query}"),
        None => endpoint.to_string(),
    }
}

// ============================================================================
// Codex API 处理器
// ============================================================================

/// 处理 /v1/chat/completions 请求（OpenAI Chat Completions API - Codex CLI）
pub async fn handle_chat_completions(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let mut ctx =
        RequestContext::new(&state, &body, &headers, AppType::Codex, "Codex", "codex").await?;
    let endpoint = endpoint_with_query(&uri, "/chat/completions");

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            &endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    ctx.provider = result.provider;
    let response = result.response;

    process_response(response, &ctx, &state, &OPENAI_PARSER_CONFIG).await
}

/// 处理 /v1/responses 请求（OpenAI Responses API - Codex CLI 透传）
///
/// 当供应商配置 `wire_api = "chat_completions"` 时，会将 Responses API 请求
/// 转换为 Chat Completions 格式发送到上游，并将响应转换回 Responses API 格式。
pub async fn handle_responses(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let mut ctx =
        RequestContext::new(&state, &body, &headers, AppType::Codex, "Codex", "codex").await?;

    // Check if the provider uses chat_completions wire_api
    let use_chat_completions = providers::CodexAdapter::is_chat_completions_mode(&ctx.provider);
    log::info!(
        "[Codex/responses] wire_api: meta.codexWireApi={:?}, use_chat_completions={}, provider={}",
        ctx.provider
            .meta
            .as_ref()
            .and_then(|m| m.codex_wire_api.as_deref()),
        use_chat_completions,
        ctx.provider.id,
    );

    let (endpoint, forward_body) = if use_chat_completions {
        // Transform Responses → Chat Completions
        // developer → system 归一化在 transform 内部处理
        let mut transformed = transform_codex_chat::responses_request_to_chat_completions(body)?;
        // 兼容性：将 system 消息合并到 user 消息（部分第三方 API 不支持 system 角色）
        merge_system_into_user(&mut transformed);
        (endpoint_with_query(&uri, "/chat/completions"), transformed)
    } else {
        // Responses API 透传：归一化 developer → system、确保 tools 有 parameters
        let body = normalize_responses_body(body);
        (endpoint_with_query(&uri, "/responses"), body)
    };

    let is_stream = forward_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            &endpoint,
            forward_body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    ctx.provider = result.provider;
    let response = result.response;

    if use_chat_completions {
        // Convert the Chat Completions response back to Responses API format
        handle_codex_chat_to_responses(response, &ctx, &state, is_stream).await
    } else {
        process_response(response, &ctx, &state, &CODEX_PARSER_CONFIG).await
    }
}

/// 处理 /v1/responses/compact 请求（OpenAI Responses Compact API - Codex CLI 透传）
///
/// 当供应商配置 `wire_api = "chat_completions"` 时，会将 Responses API 请求
/// 转换为 Chat Completions 格式发送到上游，并将响应转换回 Responses API 格式。
pub async fn handle_responses_compact(
    State(state): State<ProxyState>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let uri = parts.uri;
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    let mut ctx =
        RequestContext::new(&state, &body, &headers, AppType::Codex, "Codex", "codex").await?;

    // Check if the provider uses chat_completions wire_api
    let use_chat_completions = providers::CodexAdapter::is_chat_completions_mode(&ctx.provider);

    let (endpoint, forward_body) = if use_chat_completions {
        // Transform Responses → Chat Completions
        let mut transformed = transform_codex_chat::responses_request_to_chat_completions(body)?;
        merge_system_into_user(&mut transformed);
        (endpoint_with_query(&uri, "/chat/completions"), transformed)
    } else {
        // Responses API 透传：归一化 developer → system、确保 tools 有 parameters
        let body = normalize_responses_body(body);
        (endpoint_with_query(&uri, "/responses/compact"), body)
    };

    let is_stream = forward_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let result = match forwarder
        .forward_with_retry(
            &AppType::Codex,
            &endpoint,
            forward_body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    ctx.provider = result.provider;
    let response = result.response;

    if use_chat_completions {
        // Convert the Chat Completions response back to Responses API format
        handle_codex_chat_to_responses(response, &ctx, &state, is_stream).await
    } else {
        process_response(response, &ctx, &state, &CODEX_PARSER_CONFIG).await
    }
}

/// Handle response conversion from Chat Completions back to Responses API format.
///
/// When a Codex provider has `wire_api = "chat_completions"`, the upstream response
/// is in Chat Completions format and needs to be converted back to Responses API
/// format before returning to the Codex CLI.
async fn handle_codex_chat_to_responses(
    response: super::hyper_client::ProxyResponse,
    ctx: &RequestContext,
    state: &ProxyState,
    _is_stream: bool,
) -> Result<axum::response::Response, ProxyError> {
    let status = response.status();
    let is_sse = response.is_sse();

    if is_sse {
        // Streaming path: convert Chat Completions SSE → Responses API SSE
        let upstream_stream = response.bytes_stream();
        let context_window = ctx.provider.meta.as_ref().and_then(|m| {
            m.model_context_windows
                .as_ref()?
                .get(&ctx.request_model)
                .copied()
        });
        let responses_stream =
            create_responses_sse_stream_from_chat_completions(upstream_stream, context_window);

        // Create usage collector for logging (parse Responses API SSE events for usage)
        let usage_collector = {
            let state = state.clone();
            let provider_id = ctx.provider.id.clone();
            let model = ctx.request_model.clone();
            let status_code = status.as_u16();
            let start_time = ctx.start_time;

            SseUsageCollector::new(start_time, move |events, first_token_ms| {
                // The transformed stream emits Responses API events;
                // try to extract usage from the response.completed event.
                if let Some(usage) = TokenUsage::from_codex_stream_events_auto(&events) {
                    let latency_ms = start_time.elapsed().as_millis() as u64;
                    let state = state.clone();
                    let provider_id = provider_id.clone();
                    let model = model.clone();

                    tokio::spawn(async move {
                        log_usage(
                            &state,
                            &provider_id,
                            "codex",
                            &model,
                            &model,
                            usage,
                            latency_ms,
                            first_token_ms,
                            true,
                            status_code,
                        )
                        .await;
                    });
                } else {
                    log::debug!("[Codex/ChatCompletions] 流式响应缺少 usage 统计，跳过消费记录");
                }
            })
        };

        let timeout_config = ctx.streaming_timeout_config();

        let logged_stream = create_logged_passthrough_stream(
            responses_stream,
            "Codex/ChatCompletions",
            Some(usage_collector),
            timeout_config,
        );

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "Content-Type",
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            "Cache-Control",
            axum::http::HeaderValue::from_static("no-cache"),
        );

        let body = axum::body::Body::from_stream(logged_stream);
        Ok((headers, body).into_response())
    } else {
        // Non-streaming path: convert Chat Completions JSON → Responses API JSON
        let body_timeout =
            if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
                std::time::Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
            } else {
                std::time::Duration::ZERO
            };
        let (mut response_headers, _status, body_bytes) =
            read_decoded_body(response, ctx.tag, body_timeout).await?;

        let body_str = String::from_utf8_lossy(&body_bytes);

        let upstream_response: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
            log::error!("[Codex/ChatCompletions] 解析上游响应失败: {e}, body: {body_str}");
            ProxyError::TransformError(format!("Failed to parse upstream response: {e}"))
        })?;

        let responses_body =
            transform_codex_chat::chat_completions_response_to_responses(upstream_response)
                .map_err(|e| {
                    log::error!("[Codex/ChatCompletions] 转换响应失败: {e}");
                    e
                })?;

        // Log usage
        if let Some(usage) = TokenUsage::from_codex_response_auto(&responses_body) {
            let model = responses_body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            let latency_ms = ctx.latency_ms();

            let request_model = ctx.request_model.clone();
            tokio::spawn({
                let state = state.clone();
                let provider_id = ctx.provider.id.clone();
                let model = model.to_string();
                async move {
                    log_usage(
                        &state,
                        &provider_id,
                        "codex",
                        &model,
                        &request_model,
                        usage,
                        latency_ms,
                        None,
                        false,
                        status.as_u16(),
                    )
                    .await;
                }
            });
        }

        // Build response
        let mut builder = axum::response::Response::builder().status(status);
        strip_entity_headers_for_rebuilt_body(&mut response_headers);
        strip_hop_by_hop_response_headers(&mut response_headers);

        for (key, value) in response_headers.iter() {
            builder = builder.header(key, value);
        }

        builder = builder.header("content-type", "application/json");

        let response_body = serde_json::to_vec(&responses_body).map_err(|e| {
            log::error!("[Codex/ChatCompletions] 序列化响应失败: {e}");
            ProxyError::TransformError(format!("Failed to serialize response: {e}"))
        })?;

        let body = axum::body::Body::from(response_body);
        builder.body(body).map_err(|e| {
            log::error!("[Codex/ChatCompletions] 构建响应失败: {e}");
            ProxyError::Internal(format!("Failed to build response: {e}"))
        })
    }
}

// ============================================================================
// Gemini API 处理器
// ============================================================================

/// 处理 Gemini API 请求（透传，包括查询参数）
pub async fn handle_gemini(
    State(state): State<ProxyState>,
    uri: axum::http::Uri,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ProxyError> {
    let (parts, req_body) = request.into_parts();
    let headers = parts.headers;
    let extensions = parts.extensions;
    let body_bytes = req_body
        .collect()
        .await
        .map_err(|e| ProxyError::Internal(format!("Failed to read request body: {e}")))?
        .to_bytes();
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProxyError::Internal(format!("Failed to parse request body: {e}")))?;

    // Gemini 的模型名称在 URI 中
    let mut ctx = RequestContext::new(&state, &body, &headers, AppType::Gemini, "Gemini", "gemini")
        .await?
        .with_model_from_uri(&uri);

    // 提取完整的路径和查询参数
    let endpoint = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());

    let is_stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let forwarder = ctx.create_forwarder(&state);
    let result = match forwarder
        .forward_with_retry(
            &AppType::Gemini,
            endpoint,
            body,
            headers,
            extensions,
            ctx.get_providers(),
        )
        .await
    {
        Ok(result) => result,
        Err(mut err) => {
            if let Some(provider) = err.provider.take() {
                ctx.provider = provider;
            }
            log_forward_error(&state, &ctx, is_stream, &err.error);
            return Err(err.error);
        }
    };

    ctx.provider = result.provider;
    let response = result.response;

    process_response(response, &ctx, &state, &GEMINI_PARSER_CONFIG).await
}

fn should_use_claude_transform_streaming(
    requested_streaming: bool,
    upstream_is_sse: bool,
    api_format: &str,
    is_codex_oauth: bool,
) -> bool {
    requested_streaming || upstream_is_sse || (is_codex_oauth && api_format == "openai_responses")
}

/// 把 OpenAI Responses SSE 流聚合成一个完整的 Responses JSON 对象，供下游转成 Anthropic
/// 非流响应。仅在 Codex OAuth 把 `stream:false` 强制升级为 SSE 的场景下调用。
///
/// 复用 `proxy::sse` 的 `take_sse_block`/`strip_sse_field`：`take_sse_block` 同时支持
/// `\n\n` 与 `\r\n\r\n` 两种分隔符，`strip_sse_field` 兼容带/不带空格的字段写法。
fn responses_sse_to_response_value(body: &str) -> Result<Value, ProxyError> {
    let mut buffer = body.to_string();
    let mut completed_response: Option<Value> = None;
    let mut output_items = Vec::new();

    while let Some(block) = take_sse_block(&mut buffer) {
        let mut event_name = "";
        let mut data_lines: Vec<&str> = Vec::new();

        for line in block.lines() {
            if let Some(evt) = strip_sse_field(line, "event") {
                event_name = evt.trim();
            } else if let Some(d) = strip_sse_field(line, "data") {
                data_lines.push(d);
            }
        }

        if data_lines.is_empty() {
            continue;
        }

        let data_str = data_lines.join("\n");
        if data_str.trim() == "[DONE]" {
            continue;
        }

        let data: Value = serde_json::from_str(&data_str).map_err(|e| {
            ProxyError::TransformError(format!("Failed to parse upstream SSE event: {e}"))
        })?;

        match event_name {
            "response.output_item.done" => {
                if let Some(item) = data.get("item") {
                    output_items.push(item.clone());
                }
            }
            "response.completed" => {
                completed_response = Some(data.get("response").cloned().unwrap_or(data));
            }
            "response.failed" => {
                let message = data
                    .pointer("/response/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("response.failed event received");
                return Err(ProxyError::TransformError(message.to_string()));
            }
            _ => {}
        }
    }

    let mut response = completed_response.ok_or_else(|| {
        ProxyError::TransformError("No response.completed event in upstream SSE".to_string())
    })?;

    if !output_items.is_empty() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("output".to_string(), Value::Array(output_items));
        } else {
            return Err(ProxyError::TransformError(
                "response.completed payload is not an object".to_string(),
            ));
        }
    }

    Ok(response)
}

// ============================================================================
// 使用量记录（保留用于 Claude 转换逻辑）
// ============================================================================

fn log_forward_error(
    state: &ProxyState,
    ctx: &RequestContext,
    is_streaming: bool,
    error: &ProxyError,
) {
    use super::usage::logger::UsageLogger;

    let logger = UsageLogger::new(&state.db);
    let status_code = map_proxy_error_to_status(error);
    let error_message = get_error_message(error);
    let request_id = uuid::Uuid::new_v4().to_string();

    if let Err(e) = logger.log_error_with_context(
        request_id,
        ctx.provider.id.clone(),
        ctx.app_type_str.to_string(),
        ctx.request_model.clone(),
        status_code,
        error_message,
        ctx.latency_ms(),
        is_streaming,
        Some(ctx.session_id.clone()),
        None,
    ) {
        log::warn!("记录失败请求日志失败: {e}");
    }
}

/// 记录请求使用量
#[allow(clippy::too_many_arguments)]
async fn log_usage(
    state: &ProxyState,
    provider_id: &str,
    app_type: &str,
    model: &str,
    request_model: &str,
    usage: TokenUsage,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    is_streaming: bool,
    status_code: u16,
) {
    use super::usage::logger::UsageLogger;

    let logger = UsageLogger::new(&state.db);

    let (multiplier, pricing_model_source) =
        logger.resolve_pricing_config(provider_id, app_type).await;
    let pricing_model = if pricing_model_source == "request" {
        request_model
    } else {
        model
    };

    let request_id = usage.dedup_request_id();

    if let Err(e) = logger.log_with_calculation(
        request_id,
        provider_id.to_string(),
        app_type.to_string(),
        model.to_string(),
        request_model.to_string(),
        pricing_model.to_string(),
        usage,
        multiplier,
        latency_ms,
        first_token_ms,
        status_code,
        None,
        None, // provider_type
        is_streaming,
    ) {
        log::warn!("[USG-001] 记录使用量失败: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{responses_sse_to_response_value, should_use_claude_transform_streaming};
    use crate::proxy::ProxyError;

    #[test]
    fn codex_oauth_responses_force_streaming_even_if_client_sent_false() {
        assert!(should_use_claude_transform_streaming(
            false,
            false,
            "openai_responses",
            true,
        ));
    }

    #[test]
    fn upstream_sse_response_always_uses_streaming_path() {
        assert!(should_use_claude_transform_streaming(
            false,
            true,
            "openai_chat",
            false,
        ));
    }

    #[test]
    fn non_streaming_response_stays_non_streaming_for_regular_openai_responses() {
        assert!(!should_use_claude_transform_streaming(
            false,
            false,
            "openai_responses",
            false,
        ));
    }

    #[test]
    fn responses_sse_to_response_value_collects_output_items() {
        let sse = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","model":"gpt-5.4","output":[],"usage":{"input_tokens":10,"output_tokens":2}}}

"#;

        let response = responses_sse_to_response_value(sse).unwrap();

        assert_eq!(response["id"], "resp_1");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn responses_sse_to_response_value_handles_crlf_delimiters() {
        // 真实 HTTP SSE 按规范使用 \r\n\r\n 分隔事件；take_sse_block 必须同时处理两种分隔符，
        // 否则此路径在任何标准上游（含 Codex OAuth HTTPS 后端）下都会 TransformError。
        let sse = "event: response.output_item.done\r\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\r\n\
\r\n\
event: response.completed\r\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_crlf\",\"status\":\"completed\",\"model\":\"gpt-5.4\",\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\r\n\
\r\n";

        let response = responses_sse_to_response_value(sse).unwrap();

        assert_eq!(response["id"], "resp_crlf");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn responses_sse_to_response_value_returns_err_on_response_failed() {
        let sse = "event: response.failed\n\
data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"upstream blew up\"}}}\n\n";

        let err = responses_sse_to_response_value(sse).unwrap_err();
        match err {
            ProxyError::TransformError(msg) => assert!(msg.contains("upstream blew up")),
            other => panic!("expected TransformError, got {other:?}"),
        }
    }

    #[test]
    fn responses_sse_to_response_value_errors_when_no_completed_event() {
        let sse = "event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n\n";

        assert!(responses_sse_to_response_value(sse).is_err());
    }
}
