//! claude-multi-proxy: a local reverse proxy that routes Claude Code requests to
//! multiple LLM providers, with in-session `/model <provider>/<model>` switching.

pub mod config;
pub mod error;
pub mod model_command;
pub mod proxy;

use std::sync::Arc;
use std::time::Duration;

use config::Config;
use proxy::AppState;

/// Build shared application state (config + HTTP client) from a loaded config.
pub fn build_state(config: Config) -> Result<AppState, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.server.request_timeout_secs))
        .build()?;
    Ok(AppState {
        config: Arc::new(config),
        client,
    })
}

/// Log which provider API-key env vars are present, so a misconfiguration is
/// visible at startup rather than only when a provider is first routed to.
pub fn log_key_presence(config: &Config) {
    for (name, provider) in &config.providers {
        if provider.api_key().is_some() {
            tracing::info!(provider = %name, env = %provider.api_key_env, "API key present");
        } else {
            tracing::warn!(provider = %name, env = %provider.api_key_env, "API key NOT set");
        }
    }
}
