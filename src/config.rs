//! Configuration model and loading.
//!
//! The proxy is configured entirely from a TOML file (path taken from the
//! `PROXY_CONFIG` env var, defaulting to `./config.toml`). API keys are never
//! stored in the file -- each provider names an environment variable that holds
//! its key, resolved at request time.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// Top-level configuration, loaded once at startup and shared behind an `Arc`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub default: DefaultConfig,
    pub providers: BTreeMap<String, Provider>,
}

/// HTTP server + upstream request settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_timeout_secs")]
    pub request_timeout_secs: u64,
}

/// Provider/model used when no `/model` command is present and the request body
/// does not carry its own model.
#[derive(Debug, Clone, Deserialize)]
pub struct DefaultConfig {
    pub provider: String,
    pub model: String,
}

/// A single upstream provider: where to send requests and which env var holds
/// the API key.
#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub base_url: String,
    pub api_key_env: String,
}

impl Provider {
    /// Resolve this provider's API key from its configured environment variable.
    /// Returns `None` if the variable is unset or empty.
    pub fn api_key(&self) -> Option<String> {
        match std::env::var(&self.api_key_env) {
            Ok(v) if !v.is_empty() => Some(v),
            _ => None,
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8787
}
fn default_timeout_secs() -> u64 {
    300
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: default_host(),
            port: default_port(),
            request_timeout_secs: default_timeout_secs(),
        }
    }
}

impl Config {
    /// Parse a `Config` from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Load the config from disk.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.display().to_string(),
            source: e,
        })?;
        Self::from_toml_str(&text).map_err(ConfigError::Parse)
    }

    /// Resolve the config path from `PROXY_CONFIG`, defaulting to `config.toml`.
    pub fn resolve_path() -> String {
        std::env::var("PROXY_CONFIG").unwrap_or_else(|_| "config.toml".to_string())
    }
}

/// Errors that can occur while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [server]
        host = "0.0.0.0"
        port = 9000
        request_timeout_secs = 120

        [default]
        provider = "deepseek"
        model = "deepseek-chat"

        [providers.deepseek]
        base_url = "https://api.deepseek.com/anthropic/v1/messages"
        api_key_env = "DEEPSEEK_API_KEY"

        [providers.openrouter]
        base_url = "http://localhost:8788/v1/messages"
        api_key_env = "OPENROUTER_API_KEY"
    "#;

    #[test]
    fn parses_full_config() {
        let cfg = Config::from_toml_str(SAMPLE).expect("should parse");
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 9000);
        assert_eq!(cfg.server.request_timeout_secs, 120);
        assert_eq!(cfg.default.provider, "deepseek");
        assert_eq!(cfg.default.model, "deepseek-chat");
        assert_eq!(cfg.providers.len(), 2);
        assert_eq!(
            cfg.providers["deepseek"].base_url,
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            cfg.providers["openrouter"].api_key_env,
            "OPENROUTER_API_KEY"
        );
    }

    #[test]
    fn server_section_defaults_when_omitted() {
        let toml = r#"
            [default]
            provider = "kimi"
            model = "moonshot-v1-8k"

            [providers.kimi]
            base_url = "https://api.moonshot.ai/anthropic/v1/messages"
            api_key_env = "KIMI_API_KEY"
        "#;
        let cfg = Config::from_toml_str(toml).expect("should parse");
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8787);
        assert_eq!(cfg.server.request_timeout_secs, 300);
    }
}
