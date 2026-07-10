//! Application error type and its HTTP representation.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Errors surfaced by the proxy handler. Each maps to a specific HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Request body was not valid JSON.
    #[error("invalid JSON body: {0}")]
    InvalidJson(String),

    /// A `/model` command (or the default) named a provider not in the config.
    #[error("invalid provider specified: {0}")]
    UnknownProvider(String),

    /// The provider's configured API-key env var is unset or empty.
    #[error("API key environment variable '{env}' not set for provider '{provider}'")]
    MissingApiKey { provider: String, env: String },

    /// The request to the upstream provider failed (connect/timeout/transport).
    #[error("failed to reach provider '{provider}': {source}")]
    Upstream {
        provider: String,
        #[source]
        source: reqwest::Error,
    },
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::InvalidJson(_) => StatusCode::BAD_REQUEST,
            AppError::UnknownProvider(_) => StatusCode::BAD_REQUEST,
            AppError::MissingApiKey { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Upstream { .. } => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = self.to_string();
        if status.is_server_error() {
            tracing::error!(%status, "{message}");
        } else {
            tracing::warn!(%status, "{message}");
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}
