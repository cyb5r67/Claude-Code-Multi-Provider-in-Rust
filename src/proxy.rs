//! HTTP routing: the `/v1/messages` proxy handler and `/health`.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};

use crate::config::{AuthStyle, Config, FailMode};
use crate::error::AppError;
use crate::model_command;
use crate::orchestrator::{self, Orchestrator, Tier};
use crate::stream::{self, SentinelVerdict};

/// State shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    pub orchestrator: Option<Arc<Orchestrator>>,
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

/// Attach the provider's authentication headers to an outgoing request.
pub(crate) fn apply_auth(
    req: reqwest::RequestBuilder,
    style: AuthStyle,
    api_key: &str,
) -> reqwest::RequestBuilder {
    match style {
        AuthStyle::Bearer => req
            .header("authorization", format!("Bearer {api_key}"))
            .header("x-api-key", api_key),
        AuthStyle::Anthropic => req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
    }
}

/// Whether the request explicitly selected a provider (human override) or fell
/// through to defaults. The orchestrator only engages for `Default` routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    Default,
    Explicit,
}

/// Decide the target `(provider_key, model)` for a request and normalize the
/// payload: strips any in-text `/model` command and writes the final model back
/// into `payload["model"]`.
fn resolve_route(cfg: &Config, payload: &mut Value) -> (String, String, RouteSource) {
    // Start from the defaults; the request body may carry its own model.
    let mut provider_key = cfg.default.provider.clone();
    let mut model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.default.model)
        .to_string();
    let mut source = RouteSource::Default;

    // An in-session `/model provider/model` command in message text overrides
    // both. This is legacy behavior: current Claude Code handles `/model`
    // client-side and never sends it as text -- it sets the body's `model`
    // field to whatever the user typed, which the two branches below handle.
    if let Some(cmd) = model_command::parse_and_strip(payload) {
        tracing::info!(provider = %cmd.provider, model = %cmd.model, "model switch via /model command");
        provider_key = cmd.provider;
        model = cmd.model;
        source = RouteSource::Explicit;
    }
    // A `provider/model` value in the model field selects that provider
    // directly. Ids whose prefix is not a configured provider (e.g.
    // openrouter's `x-ai/grok-code-fast-1`) pass through untouched.
    else if let Some((prefix, rest)) = model.split_once('/') {
        if !rest.is_empty() && cfg.providers.contains_key(prefix) {
            tracing::info!(provider = %prefix, model = %rest, "model switch via model field");
            provider_key = prefix.to_string();
            model = rest.to_string();
            source = RouteSource::Explicit;
        }
    }
    // A bare provider name selects that provider's configured default model.
    else if let Some(default_model) = cfg.providers.get(&model).and_then(|p| p.model.clone()) {
        tracing::info!(provider = %model, model = %default_model, "provider switch via model field");
        provider_key = std::mem::replace(&mut model, default_model);
        source = RouteSource::Explicit;
    }

    payload["model"] = Value::String(model.clone());
    (provider_key, model, source)
}

/// Receive a Claude Code request, choose the target provider (default or via an
/// in-session `/model` command), and forward it upstream, streaming the response
/// straight back to the client.
async fn messages_proxy(State(state): State<AppState>, body: Bytes) -> Result<Response, AppError> {
    let cfg = &state.config;

    let mut payload: Value =
        serde_json::from_slice(&body).map_err(|e| AppError::InvalidJson(e.to_string()))?;

    let (provider_key, model, source) = resolve_route(cfg, &mut payload);
    tracing::info!(provider = %provider_key, %model, "routing request");

    if source == RouteSource::Default {
        if let Some(orch) = state.orchestrator.clone() {
            return cascade(&state, &orch, payload).await;
        }
    }

    forward(&state, &provider_key, &payload).await
}

/// Outcome of the local-tier attempt.
enum LocalOutcome {
    /// Deliver the local response as-is.
    Clean(Response),
    /// Sentinel detected: run the escalation path.
    Escalate,
    /// Local tier answered with an HTTP error (buffered pass-through body).
    Failed(Response),
}

/// Phase 1 sentinel cascade: try the local tier, escalate on sentinel.
async fn cascade(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    original: Value,
) -> Result<Response, AppError> {
    let key = orchestrator::conversation_key(&original);

    if let Some(k) = key.as_deref() {
        if orch.sticky_tier(k) == Some(Tier::Cloud) {
            return escalate(state, orch, key.as_deref(), &original, "sticky").await;
        }
    }

    let mut attempt = original.clone();
    set_local_model(state, orch, &mut attempt);
    orchestrator::append_system_note(
        &mut attempt,
        &orchestrator::sentinel_instruction(&orch.cfg.sentinel),
    );

    match local_attempt(state, orch, &attempt).await {
        Ok(LocalOutcome::Clean(response)) => Ok(response),
        Ok(LocalOutcome::Escalate) => {
            escalate(state, orch, key.as_deref(), &original, "sentinel").await
        }
        Ok(LocalOutcome::Failed(response)) => match orch.cfg.fail_mode {
            FailMode::Cloud => {
                tracing::warn!("local tier returned an error; escalating per fail_mode=cloud");
                escalate(state, orch, key.as_deref(), &original, "fail_mode").await
            }
            FailMode::Error => Ok(response),
        },
        Err(err) => match orch.cfg.fail_mode {
            FailMode::Cloud => {
                tracing::warn!(error = %err, "local tier unreachable; escalating per fail_mode=cloud");
                escalate(state, orch, key.as_deref(), &original, "fail_mode").await
            }
            FailMode::Error => Err(err),
        },
    }
}

/// Local attempts run against the local provider's own default model when one
/// is configured (the client's model id is meaningless to LM Studio).
fn set_local_model(state: &AppState, orch: &Arc<Orchestrator>, payload: &mut Value) {
    if let Some(local_model) = state
        .config
        .providers
        .get(&orch.cfg.local_provider)
        .and_then(|p| p.model.clone())
    {
        payload["model"] = Value::String(local_model);
    }
}

/// Route to the cloud tier, honoring the budget. On a denied budget the
/// request is answered locally with an "escalation unavailable" note.
async fn escalate(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    key: Option<&str>,
    original: &Value,
    trigger: &str,
) -> Result<Response, AppError> {
    if !orch.try_reserve_cloud_call() {
        tracing::warn!(
            trigger,
            budget_per_hour = orch.cfg.max_cloud_requests_per_hour,
            "cloud budget exhausted; answering locally"
        );
        let mut fallback = original.clone();
        set_local_model(state, orch, &mut fallback);
        orchestrator::append_system_note(&mut fallback, orchestrator::ESCALATION_UNAVAILABLE_NOTE);
        return forward(state, &orch.cfg.local_provider, &fallback).await;
    }

    if let Some(k) = key {
        orch.mark_cloud(k);
    }

    let mut cloud = original.clone();
    cloud["model"] = Value::String(orch.cfg.escalation_model.clone());
    // The audit line: one per escalation, greppable.
    tracing::info!(
        trigger,
        provider = %orch.cfg.escalation_provider,
        model = %orch.cfg.escalation_model,
        "escalating to cloud tier"
    );
    forward(state, &orch.cfg.escalation_provider, &cloud).await
}

/// Send the sentinel-instrumented attempt to the local tier and inspect the
/// leading response text. SSE inspection lands in Task 11; until then
/// streaming responses pass through as Clean.
async fn local_attempt(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    attempt: &Value,
) -> Result<LocalOutcome, AppError> {
    let provider_key = &orch.cfg.local_provider;
    let provider = state
        .config
        .providers
        .get(provider_key)
        .ok_or_else(|| AppError::UnknownProvider(provider_key.clone()))?;
    let api_key = provider.api_key().ok_or_else(|| AppError::MissingApiKey {
        provider: provider_key.clone(),
        env: provider.api_key_env.clone(),
    })?;

    let upstream = apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(attempt)
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

    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        tracing::warn!(provider = %provider_key, %status, body = %preview, "local tier returned error status");
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(LocalOutcome::Failed(response.into_response()));
    }

    let is_sse = content_type
        .to_str()
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        // Task 11 replaces this stub with sentinel scanning.
        let body_stream = upstream.bytes_stream();
        let mut response = Response::new(Body::from_stream(body_stream));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(LocalOutcome::Clean(response.into_response()));
    }

    let bytes = upstream
        .bytes()
        .await
        .map_err(|source| AppError::Upstream {
            provider: provider_key.clone(),
            source,
        })?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = stream::json_first_text(&body).unwrap_or("");
    if stream::check_sentinel(text, &orch.cfg.sentinel) == SentinelVerdict::Sentinel {
        return Ok(LocalOutcome::Escalate);
    }

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    Ok(LocalOutcome::Clean(response.into_response()))
}

/// Forward a resolved payload to the named provider, streaming the response
/// through (buffering only error bodies for logging).
pub(crate) async fn forward(
    state: &AppState,
    provider_key: &str,
    payload: &Value,
) -> Result<Response, AppError> {
    let provider = state
        .config
        .providers
        .get(provider_key)
        .ok_or_else(|| AppError::UnknownProvider(provider_key.to_string()))?;
    let api_key = provider.api_key().ok_or_else(|| AppError::MissingApiKey {
        provider: provider_key.to_string(),
        env: provider.api_key_env.clone(),
    })?;

    tracing::info!(provider = %provider_key, base_url = %provider.base_url, "forwarding request");

    let upstream = apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(payload)
    .send()
    .await
    .map_err(|source| AppError::Upstream {
        provider: provider_key.to_string(),
        source,
    })?;

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| "application/json".parse().unwrap());

    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        tracing::warn!(provider = %provider_key, %status, body = %preview, "upstream returned error status");
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(response.into_response());
    }

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
        let (provider, model, source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "alpha-default-model");
        assert_eq!(payload["model"], "alpha-default-model");
        assert_eq!(source, RouteSource::Default);
    }

    #[test]
    fn body_model_passes_through_to_default_provider() {
        let mut payload = json!({"model": "some-explicit-model", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "some-explicit-model");
    }

    #[test]
    fn provider_prefixed_model_field_switches_provider() {
        let mut payload = json!({"model": "beta/some-model", "messages": []});
        let (provider, model, source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "some-model");
        assert_eq!(payload["model"], "some-model");
        assert_eq!(source, RouteSource::Explicit);
    }

    #[test]
    fn provider_prefix_keeps_remaining_slashes_in_model() {
        // `/model beta/org/model-id` -- only the first slash separates the
        // provider; the rest is the upstream model id verbatim.
        let mut payload = json!({"model": "beta/org/model-id", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "org/model-id");
    }

    #[test]
    fn non_provider_prefix_passes_through_unchanged() {
        let mut payload = json!({"model": "x-ai/grok-code-fast-1", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "x-ai/grok-code-fast-1");
    }

    #[test]
    fn bare_provider_name_uses_configured_default_model() {
        let mut payload = json!({"model": "beta", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "beta");
        assert_eq!(model, "beta-default-model");
    }

    #[test]
    fn bare_provider_name_without_default_model_is_treated_as_model() {
        // `alpha` has no configured default model, so the string stays a model
        // id on the default provider rather than selecting provider `alpha`.
        let mut payload = json!({"model": "alpha", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "alpha");
    }

    #[test]
    fn trailing_slash_does_not_switch_provider() {
        let mut payload = json!({"model": "beta/", "messages": []});
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "alpha");
        assert_eq!(model, "beta/");
    }

    #[test]
    fn text_command_wins_over_model_field() {
        let mut payload = json!({
            "model": "beta/field-model",
            "messages": [{"role": "user", "content": "/model alpha/text-model hi"}]
        });
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
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
        let (provider, model, _source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(provider, "nope");
        assert_eq!(model, "whatever");
    }

    #[test]
    fn passthrough_model_is_default_source() {
        let mut payload = json!({"model": "x-ai/grok-code-fast-1", "messages": []});
        let (_, _, source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(source, RouteSource::Default);
    }

    #[test]
    fn text_command_is_explicit_source() {
        let mut payload = json!({
            "messages": [{"role": "user", "content": "/model beta/some-model hi"}]
        });
        let (_, _, source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(source, RouteSource::Explicit);
    }

    #[test]
    fn bare_provider_name_is_explicit_source() {
        let mut payload = json!({"model": "beta", "messages": []});
        let (_, _, source) = resolve_route(&cfg(), &mut payload);
        assert_eq!(source, RouteSource::Explicit);
    }
}
