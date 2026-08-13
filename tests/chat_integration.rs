//! Integration tests for the OpenAI-dialect chat routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use big_brother::config::Config;
use big_brother::{build_state, proxy};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

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
