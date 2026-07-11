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

/// Decide the target `(provider_key, model)` for a request and normalize the
/// payload: strips any in-text `/model` command and writes the final model back
/// into `payload["model"]`.
fn resolve_route(cfg: &Config, payload: &mut Value) -> (String, String) {
    // Start from the defaults; the request body may carry its own model.
    let mut provider_key = cfg.default.provider.clone();
    let mut model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.default.model)
        .to_string();

    // An in-session `/model provider/model` command in message text overrides
    // both. This is legacy behavior: current Claude Code handles `/model`
    // client-side and never sends it as text -- it sets the body's `model`
    // field to whatever the user typed, which the two branches below handle.
    if let Some(cmd) = model_command::parse_and_strip(payload) {
        tracing::info!(provider = %cmd.provider, model = %cmd.model, "model switch via /model command");
        provider_key = cmd.provider;
        model = cmd.model;
    }
    // A `provider/model` value in the model field selects that provider
    // directly. Ids whose prefix is not a configured provider (e.g.
    // openrouter's `x-ai/grok-code-fast-1`) pass through untouched.
    else if let Some((prefix, rest)) = model.split_once('/') {
        if !rest.is_empty() && cfg.providers.contains_key(prefix) {
            tracing::info!(provider = %prefix, model = %rest, "model switch via model field");
            provider_key = prefix.to_string();
            model = rest.to_string();
        }
    }
    // A bare provider name selects that provider's configured default model.
    else if let Some(default_model) = cfg.providers.get(&model).and_then(|p| p.model.clone()) {
        tracing::info!(provider = %model, model = %default_model, "provider switch via model field");
        provider_key = std::mem::replace(&mut model, default_model);
    }

    payload["model"] = Value::String(model.clone());
    (provider_key, model)
}

/// Receive a Claude Code request, choose the target provider (default or via an
/// in-session `/model` command), and forward it upstream, streaming the response
/// straight back to the client.
async fn messages_proxy(State(state): State<AppState>, body: Bytes) -> Result<Response, AppError> {
    let cfg = &state.config;

    let mut payload: Value =
        serde_json::from_slice(&body).map_err(|e| AppError::InvalidJson(e.to_string()))?;

    let (provider_key, model) = resolve_route(cfg, &mut payload);

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
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| "application/json".parse().unwrap());

    // Error responses are small: buffer them so the body can be logged --
    // otherwise a misconfigured upstream (wrong path, bad key, unknown model)
    // is invisible from the proxy log -- then forward them unchanged.
    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        tracing::warn!(provider = %provider_key, %status, body = %preview, "upstream returned error status");
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(response.into_response());
    }

    // Preserve the upstream status and content-type, then stream the body
    // through unbuffered (handles both streaming SSE and plain JSON responses).
    let stream = upstream.bytes_stream();
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);

    Ok(response.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Config {
        Config::from_toml_str(
            r#"
            [default]
            provider = "alpha"
            model = "alpha-default-model"

            [providers.alpha]
            base_url = "http://alpha.test/v1/messages"
            api_key_env = "ALPHA_KEY"

            [providers.beta]
            base_url = "http://beta.test/v1/messages"
            api_key_env = "BETA_KEY"
            model = "beta-default-model"
            "#,
        )
        .expect("test config parses")
    }

    #[test]
    fn defaults_apply_when_body_has_no_model() {
        let mut payload = json!({"messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "alpha-default-model");
        assert_eq!(payload["model"], "alpha-default-model");
    }

    #[test]
    fn body_model_passes_through_to_default_provider() {
        let mut payload = json!({"model": "some-explicit-model", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "some-explicit-model");
    }

    #[test]
    fn provider_prefixed_model_field_switches_provider() {
        let mut payload = json!({"model": "beta/some-model", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "some-model");
        assert_eq!(payload["model"], "some-model");
    }

    #[test]
    fn provider_prefix_keeps_remaining_slashes_in_model() {
        // `/model beta/org/model-id` -- only the first slash separates the
        // provider; the rest is the upstream model id verbatim.
        let mut payload = json!({"model": "beta/org/model-id", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "org/model-id");
    }

    #[test]
    fn non_provider_prefix_passes_through_unchanged() {
        let mut payload = json!({"model": "x-ai/grok-code-fast-1", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "x-ai/grok-code-fast-1");
    }

    #[test]
    fn bare_provider_name_uses_configured_default_model() {
        let mut payload = json!({"model": "beta", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "beta-default-model");
    }

    #[test]
    fn bare_provider_name_without_default_model_is_treated_as_model() {
        // `alpha` has no configured default model, so the string stays a model
        // id on the default provider rather than selecting provider `alpha`.
        let mut payload = json!({"model": "alpha", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "alpha");
    }

    #[test]
    fn trailing_slash_does_not_switch_provider() {
        let mut payload = json!({"model": "beta/", "messages": []});
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "beta/");
    }

    #[test]
    fn text_command_wins_over_model_field() {
        let mut payload = json!({
            "model": "beta/field-model",
            "messages": [{"role": "user", "content": "/model alpha/text-model hi"}]
        });
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "text-model");
        // Command stripped, remainder kept, final model written back.
        assert_eq!(payload["messages"][0]["content"], "hi");
        assert_eq!(payload["model"], "text-model");
    }

    #[test]
    fn unknown_provider_from_text_command_is_returned_for_later_rejection() {
        // resolve_route does not validate the provider; the handler rejects
        // unknown keys when looking them up in the config.
        let mut payload = json!({
            "messages": [{"role": "user", "content": "/model nope/whatever hi"}]
        });
        let (provider, model) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "nope");
        assert_eq!(model, "whatever");
    }
}
