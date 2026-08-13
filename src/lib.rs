//! Big Brother: a local reverse proxy that routes Claude Code requests to
//! multiple LLM providers, with in-session `/model <provider>/<model>` switching.

pub mod config;
pub mod error;
pub mod model_command;
pub mod orchestrator;
pub mod proxy;
pub mod stream;

use std::sync::Arc;
use std::time::Duration;

use config::Config;
use orchestrator::Orchestrator;
use proxy::AppState;

/// Build shared application state (config + HTTP client) from a loaded config.
pub fn build_state(config: Config) -> Result<AppState, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.server.request_timeout_secs))
        .build()?;
    let orchestrator = config
        .orchestrator
        .as_ref()
        .filter(|o| o.enabled)
        .map(|o| Arc::new(Orchestrator::new(o.clone())));
    Ok(AppState {
        config: Arc::new(config),
        client,
        orchestrator,
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

/// Log the orchestrator's startup posture so a misconfigured tier is visible
/// immediately (unknown providers still fail per-request with 400s).
pub fn log_orchestrator(config: &Config) {
    match &config.orchestrator {
        Some(o) if o.enabled => {
            for key in [&o.local_provider, &o.escalation_provider] {
                if !config.providers.contains_key(key) {
                    tracing::warn!(provider = %key, "orchestrator references unknown provider");
                }
            }
            tracing::info!(
                local = %o.local_provider,
                cloud = %o.escalation_provider,
                model = %o.escalation_model,
                budget_per_hour = o.max_cloud_requests_per_hour,
                "orchestrator enabled"
            );
        }
        Some(_) => tracing::info!("orchestrator section present but disabled"),
        None => {}
    }
}
