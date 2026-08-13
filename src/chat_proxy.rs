//! OpenAI-dialect chat routes: /v1/models, /v1/chat/completions, and the
//! panel's /chat/settings editor.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use crate::chat_settings::ChatSettings;
use crate::openai_compat::openai_error_body;
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
