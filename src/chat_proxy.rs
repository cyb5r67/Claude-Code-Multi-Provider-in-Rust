//! OpenAI-dialect chat routes: /v1/models, /v1/chat/completions, and the
//! panel's /chat/settings editor.

use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde_json::{json, Value};

use crate::chat_settings::ChatSettings;
use crate::error::AppError;
use crate::metrics;
use crate::openai_compat::{anthropic_to_openai, openai_to_anthropic, SseTranslator};
use crate::openai_compat::openai_error_body;
use crate::proxy;
use crate::proxy::AppState;

/// The single virtual model advertised to OpenAI-dialect clients. Routing is
/// decided by the panel, not the client's model picker.
pub const VIRTUAL_MODEL: &str = "big-brother";

fn openai_error(status: StatusCode, r#type: &str, message: &str) -> Response {
    (
        status,
        Json(openai_error_body(message, r#type, status.as_u16())),
    )
        .into_response()
}

/// 404 for chat routes when config.toml has no [chat] section.
fn not_configured() -> Response {
    openai_error(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "chat is not configured: add a [chat] section to config.toml",
    )
}

/// GET /v1/models -- exactly one virtual model.
pub async fn models(State(state): State<AppState>) -> Response {
    if state.chat.is_none() {
        return not_configured();
    }
    Json(json!({
        "object": "list",
        "data": [{
            "id": VIRTUAL_MODEL,
            "object": "model",
            "created": 0,
            "owned_by": "big-brother",
        }],
    }))
    .into_response()
}

/// Routing targets offered by the panel dropdown: "cascade" plus every
/// provider that has a configured default model.
fn targets(state: &AppState) -> Vec<String> {
    let mut list = vec!["cascade".to_string()];
    for (name, p) in &state.config.providers {
        if let Some(model) = &p.model {
            list.push(format!("{name}/{model}"));
        }
    }
    list
}

/// GET /chat/settings -- current settings plus the selectable targets.
pub async fn get_settings(State(state): State<AppState>) -> Response {
    let Some(chat) = &state.chat else {
        return not_configured();
    };
    let s = chat.get();
    Json(json!({
        "pipeline_enabled": s.pipeline_enabled,
        "model_override": s.model_override,
        "targets": targets(&state),
    }))
    .into_response()
}

/// POST /v1/chat/completions -- the OpenAI-dialect front door.
pub async fn chat_completions(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(chat) = state.chat.clone() else {
        return not_configured();
    };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {e}"),
            )
        }
    };
    let settings = chat.get();

    if !settings.pipeline_enabled {
        state
            .metrics
            .chat_requests_total
            .with_label_values(&[metrics::CHAT_MODE_PASSTHROUGH])
            .inc();
        return passthrough(&state, req).await;
    }

    state
        .metrics
        .chat_requests_total
        .with_label_values(&[metrics::CHAT_MODE_PIPELINE])
        .inc();
    let mut payload = match openai_to_anthropic(&req) {
        Ok(p) => p,
        Err(msg) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg),
    };

    let result = if settings.model_override == "cascade" {
        payload["model"] = Value::String(state.config.default.model.clone());
        match state.orchestrator.clone() {
            Some(orch) => proxy::cascade(&state, &orch, payload).await,
            None => proxy::forward(&state, &state.config.default.provider, &payload).await,
        }
    } else {
        // Validated at PUT time; split cannot fail for stored settings, but
        // guard anyway (a hand-edited state file may hold anything).
        match settings.model_override.split_once('/') {
            Some((provider, model)) if state.config.providers.contains_key(provider) => {
                payload["model"] = Value::String(model.to_string());
                proxy::forward(&state, provider, &payload).await
            }
            _ => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("unknown routing target '{}'", settings.model_override),
                )
            }
        }
    };

    match result {
        Ok(resp) => translate_response(resp).await,
        Err(err) => app_error_to_openai(err),
    }
}

/// Passthrough: forward the OpenAI request to the configured local endpoint.
/// Replaced with the real implementation in the passthrough task.
async fn passthrough(_state: &AppState, _req: Value) -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "api_error",
        "passthrough not yet implemented",
    )
}

/// Convert the pipeline's AppError into an OpenAI-shaped error response.
fn app_error_to_openai(err: AppError) -> Response {
    let plain = err.into_response();
    let status = plain.status();
    let r#type = if status.is_server_error() {
        "api_error"
    } else {
        "invalid_request_error"
    };
    openai_error(status, r#type, &format!("upstream pipeline error ({status})"))
}

/// Translate a pipeline response (Anthropic dialect) to the OpenAI dialect.
/// SSE bodies stream through `SseTranslator`; JSON bodies are buffered and
/// converted; non-success statuses become OpenAI-shaped errors.
async fn translate_response(resp: Response) -> Response {
    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    if !status.is_success() {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap_or_default();
        let detail = String::from_utf8_lossy(&bytes);
        let r#type = if status.is_server_error() {
            "api_error"
        } else {
            "invalid_request_error"
        };
        return openai_error(status, r#type, &format!("upstream error: {detail}"));
    }

    if is_sse {
        let translator = Arc::new(Mutex::new(SseTranslator::new()));
        let map_t = translator.clone();
        let mapped = resp.into_body().into_data_stream().map(move |chunk| {
            chunk.map(|b| axum::body::Bytes::from(map_t.lock().unwrap().push(&b)))
        });
        let tail = futures_util::stream::once(async move {
            Ok(axum::body::Bytes::from(translator.lock().unwrap().finish()))
        });
        let mut out = Response::new(Body::from_stream(mapped.chain(tail)));
        *out.status_mut() = status;
        out.headers_mut()
            .insert(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        return out;
    }

    // Buffered JSON translation. 8 MB cap: chat completions are small.
    match axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => Json(anthropic_to_openai(&v)).into_response(),
            Err(e) => openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream returned unparseable JSON: {e}"),
            ),
        },
        Err(e) => openai_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("failed to read upstream body: {e}"),
        ),
    }
}

/// PUT /chat/settings -- validate, apply, persist. Body: ChatSettings JSON.
pub async fn put_settings(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(chat) = &state.chat else {
        return not_configured();
    };
    let new: ChatSettings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid settings body: {e}"),
            )
        }
    };
    // "cascade" or "<known-provider>/<model>" only.
    if new.model_override != "cascade" {
        let valid = new
            .model_override
            .split_once('/')
            .is_some_and(|(p, m)| !m.is_empty() && state.config.providers.contains_key(p));
        if !valid {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("unknown routing target '{}'", new.model_override),
            );
        }
    }
    if let Err(e) = chat.set(new) {
        // In-memory state is updated; only persistence failed.
        tracing::error!(error = %e, "failed to persist chat settings");
        return openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            &format!("settings applied but not persisted: {e}"),
        );
    }
    get_settings(State(state)).await
}
