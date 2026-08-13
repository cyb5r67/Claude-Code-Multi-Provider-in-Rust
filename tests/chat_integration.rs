//! Integration tests for the OpenAI-dialect chat routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use big_brother::config::Config;
use big_brother::{build_state, proxy};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Config with a [chat] section; state_file is unique per test to avoid races.
fn chat_config(upstream: &str, state_file: &str) -> String {
    std::env::set_var("CHAT_IT_KEY", "chat-secret");
    format!(
        r#"
        [default]
        provider = "primary"
        model = "primary-default-model"

        [chat]
        passthrough_url = "{upstream}/v1/chat/completions"
        passthrough_model = "local-model"
        state_file = "{state_file}"

        [providers.primary]
        base_url = "{upstream}/v1/messages"
        api_key_env = "CHAT_IT_KEY"
        model = "primary-default-model"
        "#
    )
}

fn temp_state(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("bb_chat_it_{name}_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    // TOML string: forward slashes work on Windows too and need no escaping.
    p.to_str().unwrap().replace('\\', "/")
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn put(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn models_lists_single_virtual_model() {
    let cfg =
        Config::from_toml_str(&chat_config("http://unused.test", &temp_state("models"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = get(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["id"], "big-brother");
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn chat_routes_404_openai_shaped_without_chat_config() {
    let toml = r#"
        [default]
        provider = "a"
        model = "m"

        [providers.a]
        base_url = "http://a.test/v1/messages"
        api_key_env = "A_KEY"
    "#;
    let app = proxy::router(build_state(Config::from_toml_str(toml).unwrap()).unwrap());
    let (status, body) = get(app, "/v1/models").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["message"].as_str().unwrap().contains("[chat]"));
}

#[tokio::test]
async fn settings_round_trip_and_validation() {
    let state_file = temp_state("settings");
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &state_file)).unwrap();
    let state = build_state(cfg).unwrap();

    let (status, body) = get(proxy::router(state.clone()), "/chat/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pipeline_enabled"], true);
    assert_eq!(body["model_override"], "cascade");
    assert_eq!(
        body["targets"],
        json!(["cascade", "primary/primary-default-model"])
    );

    // Valid update persists to the state file and is reflected in GET.
    let (status, body) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": false, "model_override": "primary/other-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pipeline_enabled"], false);
    assert_eq!(body["model_override"], "primary/other-model");
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(on_disk["model_override"], "primary/other-model");

    // Unknown provider target is rejected.
    let (status, body) = put(
        proxy::router(state),
        "/chat/settings",
        json!({"pipeline_enabled": true, "model_override": "nope/model"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("nope/model"));
    let _ = std::fs::remove_file(&state_file);
}

async fn post_chat(app: axum::Router, body: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn pipeline_chat_translates_both_directions() {
    let server = MockServer::start().await;
    // The upstream must receive an ANTHROPIC-dialect request with the default
    // model (no orchestrator configured -> forward to default provider).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "model": "primary-default-model",
            "system": "Be terse.",
            "messages": [{"role": "user", "content": "hi"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_7", "model": "primary-default-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello back"}],
            "usage": {"input_tokens": 3, "output_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &temp_state("pipe"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(
        app,
        json!({
            "model": "big-brother",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "hi"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "hello back");
    assert_eq!(v["usage"]["total_tokens"], 5);
}

#[tokio::test]
async fn pipeline_chat_streams_openai_chunks() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"model\":\"m\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &temp_state("sse"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(
        app,
        json!({"model": "big-brother", "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("chat.completion.chunk"), "body: {body}");
    assert!(body.contains("\"content\":\"Hi\""), "body: {body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "body: {body}");
}

#[tokio::test]
async fn explicit_model_override_routes_directly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "special-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_o", "model": "special-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let state_file = temp_state("override");
    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &state_file)).unwrap();
    let state = build_state(cfg).unwrap();
    let (status, _) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": true, "model_override": "primary/special-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_chat(
        proxy::router(state),
        json!({"model": "big-brother", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok");
    let _ = std::fs::remove_file(&state_file);
}

#[tokio::test]
async fn chat_escalates_through_cascade_on_sentinel() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_l", "model": "local-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "<<ESCALATE>>"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&local)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "cloud-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_c", "model": "cloud-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "cloud answer"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&cloud)
        .await;

    std::env::set_var("CHAT_IT_LOCAL_KEY", "k1");
    std::env::set_var("CHAT_IT_CLOUD_KEY", "k2");
    let toml = format!(
        r#"
        [default]
        provider = "local"
        model = "local-model"

        [chat]
        passthrough_url = "{lu}/v1/chat/completions"
        passthrough_model = "local-model"
        state_file = "{state}"

        [orchestrator]
        local_provider = "local"
        escalation_provider = "cloud"
        escalation_model = "cloud-model"

        [providers.local]
        base_url = "{lu}/v1/messages"
        api_key_env = "CHAT_IT_LOCAL_KEY"
        model = "local-model"

        [providers.cloud]
        base_url = "{cu}/v1/messages"
        api_key_env = "CHAT_IT_CLOUD_KEY"
        model = "cloud-model"
        "#,
        lu = local.uri(),
        cu = cloud.uri(),
        state = temp_state("cascade"),
    );
    let app = proxy::router(build_state(Config::from_toml_str(&toml).unwrap()).unwrap());
    let (status, body) = post_chat(
        app,
        json!({"model": "big-brother", "messages": [{"role": "user", "content": "hard question"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "cloud answer");
}

#[tokio::test]
async fn malformed_chat_request_is_400_openai_shaped() {
    let cfg =
        Config::from_toml_str(&chat_config("http://unused.test", &temp_state("bad"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(app, json!({"model": "big-brother"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error");
}
