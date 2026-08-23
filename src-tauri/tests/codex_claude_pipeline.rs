//! End-to-end integration tests for the Codex→Claude pipeline.
//!
//! These tests start a real proxy server, configure a Claude provider
//! pointing to the upstream Anthropic-compatible API, enable the
//! `codex_use_claude_pipeline` toggle, and send Codex-format requests
//! to `/v1/responses`. The full pipeline is exercised:
//!
//! Codex request → proxy → responses_request_to_anthropic → forwarder
//! → upstream API → streaming_anthropic_to_responses / anthropic_response_to_responses
//! → Codex response
//!
//! Required env vars:
//!   ANTHROPIC_TEST_API_KEY or JDCLOUD_API_KEY
//!   ANTHROPIC_TEST_BASE_URL  (default: https://api.anthropic.com)
//!   ANTHROPIC_TEST_MODEL     (default: claude-sonnet-4-20250514)
//!   ANTHROPIC_TEST_AUTH_HEADER (default: x-api-key; use "bearer" for JDCloud)

use std::sync::Arc;

use cc_switch_lib::{
    start_test_proxy, update_settings, AppSettings, CodexClaudePipelineMode, Database, Provider,
};
use serde_json::{json, Value};

#[path = "support.rs"]
mod support;
use support::{ensure_test_home, reset_test_fs, test_mutex};

fn integration_config() -> (String, String, String) {
    let api_key = std::env::var("ANTHROPIC_TEST_API_KEY")
        .or_else(|_| std::env::var("JDCLOUD_API_KEY"))
        .expect("ANTHROPIC_TEST_API_KEY or JDCLOUD_API_KEY env var required");
    let base_url = std::env::var("ANTHROPIC_TEST_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let model = std::env::var("ANTHROPIC_TEST_MODEL")
        .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
    (api_key, base_url, model)
}

fn create_claude_provider(api_key: &str, base_url: &str) -> Provider {
    let auth_header =
        std::env::var("ANTHROPIC_TEST_AUTH_HEADER").unwrap_or_else(|_| "x-api-key".to_string());

    let env = if auth_header == "bearer" {
        json!({
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_AUTH_TOKEN": api_key
        })
    } else {
        json!({
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_API_KEY": api_key
        })
    };

    Provider {
        id: "test-claude-jdcloud".to_string(),
        name: "Test Claude Provider".to_string(),
        settings_config: json!({ "env": env }),
        website_url: None,
        category: None,
        created_at: None,
        sort_index: None,
        notes: None,
        meta: None,
        icon: None,
        icon_color: None,
        in_failover_queue: false,
    }
}

async fn setup_proxy() -> (u16, Arc<Database>) {
    let (api_key, base_url, _model) = integration_config();

    let db = Arc::new(Database::init().expect("create test database"));

    // Insert a Claude provider pointing to the upstream API
    let provider = create_claude_provider(&api_key, &base_url);
    db.save_provider("claude", &provider)
        .expect("save provider");

    // Mark it as current
    let mut settings = AppSettings::default();
    settings.codex_use_claude_pipeline = CodexClaudePipelineMode::Always;
    settings.current_provider_claude = Some(provider.id.clone());
    update_settings(settings).expect("update settings");

    let (port, _server) = start_test_proxy(db.clone()).await.expect("start proxy");

    // Keep server alive by leaking it (tests are short-lived)
    std::mem::forget(_server);

    (port, db)
}

async fn post_responses(port: u16, body: &Value) -> (u16, String) {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .expect("HTTP request to proxy failed");

    let status = resp.status().as_u16();
    let text = resp.text().await.expect("read response body");
    (status, text)
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore]
async fn e2e_simple_text() {
    let _guard = test_mutex().lock().expect("acquire test mutex");
    reset_test_fs();
    let _home = ensure_test_home();

    let (_model_name, _, model) = (
        integration_config().0,
        integration_config().1,
        integration_config().2,
    );
    let (port, _db) = setup_proxy().await;

    let body = json!({
        "model": model,
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Reply with exactly: e2e works"}]}
        ],
        "max_output_tokens": 256
    });

    let (status, text) = post_responses(port, &body).await;
    assert!(
        status == 200,
        "Expected 200, got {status}. Body: {}",
        &text[..text.len().min(1000)]
    );

    let resp: Value = serde_json::from_str(&text).expect("response is not JSON");
    assert_eq!(resp["status"], "completed", "Response: {resp}");
    assert!(
        !resp["output"].as_array().unwrap().is_empty(),
        "Empty output: {resp}"
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore]
async fn e2e_developer_role() {
    let _guard = test_mutex().lock().expect("acquire test mutex");
    reset_test_fs();
    let _home = ensure_test_home();

    let (_, _, model) = integration_config();
    let (port, _db) = setup_proxy().await;

    let body = json!({
        "model": model,
        "input": [
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "You are a helpful assistant."}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Say hi"}]}
        ],
        "max_output_tokens": 256
    });

    let (status, text) = post_responses(port, &body).await;
    assert!(
        status == 200,
        "Expected 200 (developer role should be mapped to user), got {status}. Body: {}",
        &text[..text.len().min(1000)]
    );

    let resp: Value = serde_json::from_str(&text).expect("response is not JSON");
    assert_eq!(resp["status"], "completed", "Response: {resp}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore]
async fn e2e_streaming() {
    let _guard = test_mutex().lock().expect("acquire test mutex");
    reset_test_fs();
    let _home = ensure_test_home();

    let (_, _, model) = integration_config();
    let (port, _db) = setup_proxy().await;

    let body = json!({
        "model": model,
        "input": "Reply with exactly: streaming e2e",
        "max_output_tokens": 256,
        "stream": true
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("HTTP request to proxy failed");

    let status = resp.status().as_u16();
    let text = resp.text().await.expect("read response body");

    assert!(
        status == 200,
        "Expected 200 for streaming, got {status}. Body: {}",
        &text[..text.len().min(1000)]
    );

    // Verify SSE events are present (converted from Anthropic to Responses format)
    assert!(
        text.contains("response.created") || text.contains("response.output_text.delta"),
        "Missing Responses SSE events in stream. Got: {}",
        &text[..text.len().min(2000)]
    );
    assert!(
        text.contains("response.completed"),
        "Missing response.completed in stream. Got: {}",
        &text[..text.len().min(2000)]
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
#[ignore]
async fn e2e_tool_use() {
    let _guard = test_mutex().lock().expect("acquire test mutex");
    reset_test_fs();
    let _home = ensure_test_home();

    let (_, _, model) = integration_config();
    let (port, _db) = setup_proxy().await;

    let body = json!({
        "model": model,
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Get the weather in Tokyo. You must use the get_weather tool."}]}
        ],
        "tools": [
            {
                "type": "function",
                "name": "get_weather",
                "description": "Get current weather for a city",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "description": "City name"}
                    },
                    "required": ["city"]
                }
            }
        ],
        "tool_choice": "required",
        "max_output_tokens": 1024
    });

    let (status, text) = post_responses(port, &body).await;
    assert!(
        status == 200,
        "Expected 200 for tool use, got {status}. Body: {}",
        &text[..text.len().min(1000)]
    );

    let resp: Value = serde_json::from_str(&text).expect("response is not JSON");
    let output = resp["output"].as_array().expect("output should be array");
    let has_function_call = output.iter().any(|item| item["type"] == "function_call");
    assert!(
        has_function_call,
        "Expected function_call in output: {output:?}"
    );
}
