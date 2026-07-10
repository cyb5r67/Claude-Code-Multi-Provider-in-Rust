//! HTTP routing: the `/v1/messages` proxy handler and `/health`.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};

use crate::config::Config;
use crate::error::AppError;
use crate::model_command;

/// State shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
}

/// Build the axum router. Kept separate from server startup so tests can drive
/// it directly with `tower::ServiceExt::oneshot`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(messages_proxy))
        .route("/health", get(health))
        .with_state(state)
}

/// Simple health check.
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Receive a Claude Code request, choose the target provider (default or via an
/// in-session `/model` command), and forward it upstream, streaming the response
/// straight back to the client.
async fn messages_proxy(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, AppError> {
    let cfg = &state.config;

    let mut payload: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::InvalidJson(e.to_string()))?;

    // Start from the defaults; the request body may carry its own model.
    let mut provider_key = cfg.default.provider.clone();
    let mut model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.default.model)
        .to_string();

    // An in-session `/model provider/model` command overrides both.
    if let Some(cmd) = model_command::parse_and_strip(&mut payload) {
        tracing::info!(provider = %cmd.provider, model = %cmd.model, "model switch via /model command");
        provider_key = cmd.provider;
        model = cmd.model;
    }

    payload["model"] = Value::String(model.clone());

    // Resolve provider + API key.
    let provider = cfg
        .providers
        .get(&provider_key)
        .ok_or_else(|| AppError::UnknownProvider(provider_key.clone()))?;
    let api_key = provider.api_key().ok_or_else(|| AppError::MissingApiKey {
        provider: provider_key.clone(),
        env: provider.api_key_env.clone(),
    })?;

    tracing::info!(provider = %provider_key, %model, base_url = %provider.base_url, "routing request");

    // Forward upstream. `.json()` serializes the (mutated) payload and sets
    // Content-Type; we add the auth headers the various providers expect.
    let upstream = state
        .client
        .post(&provider.base_url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("x-api-key", &api_key) // y-router compatibility
        .json(&payload)
        .send()
        .await
        .map_err(|source| AppError::Upstream {
            provider: provider_key.clone(),
            source,
        })?;

    let status = upstream.status();
    if !status.is_success() {
        tracing::warn!(provider = %provider_key, %status, "upstream returned error status");
    }

    // Preserve the upstream status and content-type, then stream the body
    // through unbuffered (handles both streaming SSE and plain JSON responses).
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| "application/json".parse().unwrap());

    let stream = upstream.bytes_stream();
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, content_type);

    Ok(response.into_response())
}
