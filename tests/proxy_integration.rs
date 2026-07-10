//! End-to-end tests driving the axum router against a mocked upstream provider.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claude_multi_proxy::config::Config;
use claude_multi_proxy::{build_state, proxy};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`
use wiremock::matchers::{body_json, header, method, path};
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
