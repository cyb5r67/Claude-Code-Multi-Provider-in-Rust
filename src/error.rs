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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use serde_json::Value;

    /// Render an error the way the handler does and return (status, body).
    async fn render(err: AppError) -> (StatusCode, Value) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Build a real `reqwest::Error` without touching the network: sending a
    /// request with an unparseable URL fails at build time.
    async fn reqwest_error() -> reqwest::Error {
        reqwest::Client::new()
            .get("not a url")
            .send()
            .await
            .expect_err("invalid URL must fail")
    }

    #[tokio::test]
    async fn invalid_json_is_400_with_json_error_body() {
        let (status, body) = render(AppError::InvalidJson("boom".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid JSON body: boom");
    }

    #[tokio::test]
    async fn unknown_provider_is_400_and_names_the_provider() {
        let (status, body) = render(AppError::UnknownProvider("nope".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid provider specified: nope");
    }

    #[tokio::test]
    async fn missing_api_key_is_500_and_names_provider_and_env() {
        let (status, body) = render(AppError::MissingApiKey {
            provider: "qwen".into(),
            env: "LMSTUDIO".into(),
        })
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["error"],
            "API key environment variable 'LMSTUDIO' not set for provider 'qwen'"
        );
    }

    #[tokio::test]
    async fn upstream_failure_is_502_and_names_the_provider() {
        let (status, body) = render(AppError::Upstream {
            provider: "deepseek".into(),
            source: reqwest_error().await,
        })
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let message = body["error"].as_str().unwrap();
        assert!(
            message.starts_with("failed to reach provider 'deepseek'"),
            "unexpected message: {message}"
        );
    }
}
