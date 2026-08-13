//! End-to-end tests driving the axum router against a mocked upstream provider.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use big_brother::config::Config;
use big_brother::{build_state, proxy};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use wiremock::matchers::{body_json, body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a router whose providers point at the given mock server, and set the
/// API-key env vars the config references.
fn config_toml(primary_url: &str, secondary_url: &str) -> String {
    // Unique env var names per test avoid cross-test interference.
    std::env::set_var("IT_PRIMARY_KEY", "primary-secret");
    std::env::set_var("IT_SECONDARY_KEY", "secondary-secret");
    format!(
        r#"
        [server]
        host = "127.0.0.1"
        port = 8787
        request_timeout_secs = 30

        [default]
        provider = "primary"
        model = "primary-default-model"

        [providers.primary]
        base_url = "{primary_url}/v1/messages"
        api_key_env = "IT_PRIMARY_KEY"

        [providers.secondary]
        base_url = "{secondary_url}/v1/messages"
        api_key_env = "IT_SECONDARY_KEY"
        model = "secondary-default-model"
        "#
    )
}

async fn send(app: axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn routes_to_default_provider_injects_headers_and_passes_through() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer primary-secret"))
        .and(header("x-api-key", "primary-secret"))
        // The body model is preserved when no /model command is present.
        .and(body_json(json!({
            "model": "some-explicit-model",
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "id": "abc"})))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "some-explicit-model",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ok": true, "id": "abc"}));
}

#[tokio::test]
async fn model_command_reroutes_and_strips_command() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    // Primary must NOT be called once the /model command reroutes to secondary.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&primary)
        .await;

    // Secondary receives the switched model and the stripped message.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secondary-secret"))
        .and(body_json(json!({
            "model": "switched-model",
            "messages": [{"role": "user", "content": "do the thing"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "secondary"})))
        .expect(1)
        .mount(&secondary)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&primary.uri(), &secondary.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "primary-default-model",
            "messages": [{"role": "user", "content": "/model secondary/switched-model do the thing"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "secondary"}));
}

/// Claude Code's built-in `/model` sets the request body's `model` field
/// rather than sending the command as message text; a `provider/model` value
/// there must switch providers.
#[tokio::test]
async fn provider_prefixed_model_field_reroutes() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secondary-secret"))
        .and(body_json(json!({
            "model": "switched-model",
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "secondary"})))
        .expect(1)
        .mount(&secondary)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&primary.uri(), &secondary.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "secondary/switched-model",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "secondary"}));
}

/// A bare provider name in the model field selects that provider with its
/// configured default model.
#[tokio::test]
async fn bare_provider_name_uses_its_default_model() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secondary-secret"))
        .and(body_json(json!({
            "model": "secondary-default-model",
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "secondary"})))
        .expect(1)
        .mount(&secondary)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&primary.uri(), &secondary.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "secondary",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "secondary"}));
}

/// A slash-containing model id whose prefix is NOT a configured provider
/// (e.g. openrouter's `x-ai/...` ids) passes through to the default provider
/// unchanged.
#[tokio::test]
async fn non_provider_slash_model_passes_through_to_default() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "primary-secret"))
        .and(body_json(json!({
            "model": "x-ai/grok-code-fast-1",
            "messages": [{"role": "user", "content": "hello"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "primary"})))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "x-ai/grok-code-fast-1",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "primary"}));
}

/// Upstream HTTP errors are forwarded with their original status and body
/// (and logged with the body for diagnosability) -- never remapped.
#[tokio::test]
async fn upstream_error_status_and_body_pass_through() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": "Unexpected endpoint or method"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "some-model",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Unexpected endpoint or method"}));
}

#[tokio::test]
async fn unknown_provider_returns_400() {
    let server = MockServer::start().await;
    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "messages": [{"role": "user", "content": "/model nope/whatever hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("nope"));
}

/// Providers with auth_style = "anthropic" get x-api-key + anthropic-version
/// headers (api.anthropic.com rejects requests without the version header).
#[tokio::test]
async fn anthropic_auth_style_sends_version_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "anthropic-secret"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "anthropic"})))
        .expect(1)
        .mount(&server)
        .await;

    std::env::set_var("IT_ANTHROPIC_KEY", "anthropic-secret");
    let toml = format!(
        r#"
        [default]
        provider = "anthropic"
        model = "claude-opus-5"

        [providers.anthropic]
        base_url = "{}/v1/messages"
        api_key_env = "IT_ANTHROPIC_KEY"
        auth_style = "anthropic"
        "#,
        server.uri()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "anthropic"}));
}

use big_brother::orchestrator::{sentinel_instruction, ESCALATION_UNAVAILABLE_NOTE};

/// Config with the orchestrator enabled: "local" is the Qwen stand-in,
/// "cloud" the Anthropic stand-in.
fn orchestrated_config_toml(
    local_url: &str,
    cloud_url: &str,
    max_per_hour: u32,
    fail_mode: &str,
) -> String {
    std::env::set_var("IT_LOCAL_KEY", "local-secret");
    std::env::set_var("IT_CLOUD_KEY", "cloud-secret");
    format!(
        r#"
        [default]
        provider = "local"
        model = "local-default-model"

        [orchestrator]
        local_provider = "local"
        escalation_provider = "cloud"
        escalation_model = "cloud-big-model"
        max_cloud_requests_per_hour = {max_per_hour}
        fail_mode = "{fail_mode}"

        [providers.local]
        base_url = "{local_url}/v1/messages"
        api_key_env = "IT_LOCAL_KEY"
        model = "local-model"

        [providers.cloud]
        base_url = "{cloud_url}/v1/messages"
        api_key_env = "IT_CLOUD_KEY"
        auth_style = "anthropic"
        "#
    )
}

fn sentinel_json_response() -> Value {
    json!({"content": [{"type": "text", "text": "<<ESCALATE>>"}]})
}

#[tokio::test]
async fn sentinel_response_escalates_to_cloud() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Local attempt: model overridden to the local default, sentinel
    // instruction injected as the system prompt.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "model": "local-model",
            "system": sentinel_instruction("<<ESCALATE>>")
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(1)
        .mount(&local)
        .await;

    // Escalation: the ORIGINAL payload (no injected system prompt) with the
    // model swapped to the escalation model, Anthropic-style headers.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-api-key", "cloud-secret"))
        .and(body_json(json!({
            "model": "cloud-big-model",
            "messages": [{"role": "user", "content": "hard question"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "whatever-model",
            "messages": [{"role": "user", "content": "hard question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn clean_response_is_answered_locally() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"content": [{"type": "text", "text": "The answer is 4."}]})),
        )
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "whatever-model",
            "messages": [{"role": "user", "content": "what is 2+2"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"][0]["text"], "The answer is 4.");
}

#[tokio::test]
async fn escalated_conversation_is_sticky() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Local is attempted exactly once (turn 1); turn 2 skips it.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(2)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let state = build_state(cfg).unwrap();

    let turn1 = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hard question"}]
    });
    let turn2 = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "hard question"},
            {"role": "assistant", "content": "cloud answer"},
            {"role": "user", "content": "follow-up"}
        ]
    });

    let (s1, _) = send(proxy::router(state.clone()), turn1).await;
    let (s2, b2) = send(proxy::router(state), turn2).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn exhausted_budget_falls_back_to_local_with_note() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Budget-denied fallback: original payload + escalation-unavailable note.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(
            json!({"system": ESCALATION_UNAVAILABLE_NOTE}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"content": [{"type": "text", "text": "best local effort"}]})),
        )
        .expect(1)
        .mount(&local)
        .await;

    // Sentinel attempts (conversations A and B).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(2)
        .mount(&local)
        .await;

    // Budget of 1: only conversation A gets through.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        1,
        "cloud",
    ))
    .unwrap();
    let state = build_state(cfg).unwrap();

    let conv_a = json!({"model": "m", "messages": [{"role": "user", "content": "conversation A"}]});
    let conv_b = json!({"model": "m", "messages": [{"role": "user", "content": "conversation B"}]});

    let (sa, ba) = send(proxy::router(state.clone()), conv_a).await;
    let (sb, bb) = send(proxy::router(state), conv_b).await;

    assert_eq!(sa, StatusCode::OK);
    assert_eq!(ba, json!({"routed": "cloud"}));
    assert_eq!(sb, StatusCode::OK);
    assert_eq!(bb["content"][0]["text"], "best local effort");
}

/// Explicit provider selection bypasses the cascade entirely.
#[tokio::test]
async fn explicit_model_selection_bypasses_orchestrator() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_json(json!({
            "model": "picked-model",
            "messages": [{"role": "user", "content": "hi"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud-explicit"})))
        .expect(1)
        .mount(&cloud)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&local)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    // `cloud/picked-model` explicitly names the cloud provider.
    let (status, body) = send(
        app,
        json!({
            "model": "cloud/picked-model",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud-explicit"}));
}

fn sse_body(first_text: &str, rest: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\"}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{first_text}\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{rest}\"}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Send and return the raw body string (for SSE responses).
async fn send_raw(app: axum::Router, body: Value) -> (StatusCode, String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).to_string(),
        content_type,
    )
}

#[tokio::test]
async fn sse_sentinel_escalates_to_cloud() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body("<<ESCALATE>>", ""), "text/event-stream"),
        )
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "cloud-big-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "hard streaming question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn sse_clean_response_streams_through_verbatim() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    let full_body = sse_body("Hello", " there");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(full_body.clone(), "text/event-stream"),
        )
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&cloud)
        .await;

    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        &local.uri(),
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body, content_type) = send_raw(
        app,
        json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "easy streaming question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    // Every byte the local tier produced reaches the client unmodified.
    assert_eq!(body, full_body);
}
