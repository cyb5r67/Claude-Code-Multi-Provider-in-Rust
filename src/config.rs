//! Configuration model and loading.
//!
//! The proxy is configured entirely from a TOML file (path taken from the
//! `PROXY_CONFIG` env var, defaulting to `./config.toml`). API keys are never
//! stored in the file -- each provider names an environment variable that holds
//! its key, resolved at request time.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Top-level configuration, loaded once at startup and shared behind an `Arc`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub default: DefaultConfig,
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,
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
    /// Optional default model, used when a request selects this provider by
    /// bare name (e.g. Claude Code's `/model qwen`).
    #[serde(default)]
    pub model: Option<String>,
    /// How to authenticate; defaults to the Bearer style used by y-router-like
    /// endpoints.
    #[serde(default)]
    pub auth_style: AuthStyle,
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

/// How to authenticate against a provider's endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthStyle {
    /// `authorization: Bearer <key>` plus `x-api-key` (y-router compatibility).
    #[default]
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version: 2023-06-01` (api.anthropic.com).
    Anthropic,
}

/// Behavior when the local tier fails before a sentinel verdict is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
    /// Escalate to the cloud tier (budget permitting).
    #[default]
    Cloud,
    /// Surface the local tier's error to the client (pre-orchestrator behavior).
    Error,
}

/// Settings for the hierarchical orchestrator (Phase 1 sentinel cascade).
#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    /// Presence of the section implies intent, so this defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub local_provider: String,
    pub escalation_provider: String,
    pub escalation_model: String,
    #[serde(default = "default_sentinel")]
    pub sentinel: String,
    #[serde(default = "default_max_cloud_requests_per_hour")]
    pub max_cloud_requests_per_hour: u32,
    #[serde(default)]
    pub fail_mode: FailMode,
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
fn default_true() -> bool {
    true
}
fn default_sentinel() -> String {
    "<<ESCALATE>>".to_string()
}
fn default_max_cloud_requests_per_hour() -> u32 {
    50
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

    /// Resolve the config path: an explicit CLI argument wins, then the
    /// `PROXY_CONFIG` env var, then `config.toml`.
    pub fn resolve_path(cli_arg: Option<String>) -> String {
        cli_arg
            .or_else(|| std::env::var("PROXY_CONFIG").ok())
            .unwrap_or_else(|| "config.toml".to_string())
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

    fn provider(api_key_env: &str) -> Provider {
        Provider {
            base_url: "http://example.test/v1/messages".into(),
            api_key_env: api_key_env.into(),
            model: None,
            auth_style: AuthStyle::default(),
        }
    }

    // Each api_key test uses its own env var name: unit tests run in parallel
    // threads within one process, so shared names would race.
    #[test]
    fn api_key_resolves_when_env_var_is_set() {
        std::env::set_var("CMP_TEST_KEY_SET", "secret");
        assert_eq!(
            provider("CMP_TEST_KEY_SET").api_key().as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn api_key_is_none_when_env_var_is_empty() {
        std::env::set_var("CMP_TEST_KEY_EMPTY", "");
        assert_eq!(provider("CMP_TEST_KEY_EMPTY").api_key(), None);
    }

    #[test]
    fn api_key_is_none_when_env_var_is_unset() {
        assert_eq!(provider("CMP_TEST_KEY_NEVER_SET").api_key(), None);
    }

    #[test]
    fn provider_model_is_optional() {
        let toml = r#"
            [default]
            provider = "a"
            model = "m"

            [providers.a]
            base_url = "http://a.test/v1/messages"
            api_key_env = "A_KEY"
            model = "a-default"

            [providers.b]
            base_url = "http://b.test/v1/messages"
            api_key_env = "B_KEY"
        "#;
        let cfg = Config::from_toml_str(toml).expect("should parse");
        assert_eq!(cfg.providers["a"].model.as_deref(), Some("a-default"));
        assert_eq!(cfg.providers["b"].model, None);
    }

    // One sequential test for all three branches: PROXY_CONFIG is shared
    // process state, so splitting these into parallel tests would race.
    #[test]
    fn resolve_path_prefers_arg_then_env_then_default() {
        std::env::set_var("PROXY_CONFIG", "from-env.toml");
        assert_eq!(
            Config::resolve_path(Some("from-arg.toml".into())),
            "from-arg.toml"
        );
        assert_eq!(Config::resolve_path(None), "from-env.toml");
        std::env::remove_var("PROXY_CONFIG");
        assert_eq!(Config::resolve_path(None), "config.toml");
    }

    #[test]
    fn missing_required_section_fails_to_parse() {
        // No [default] section.
        let toml = r#"
            [providers.a]
            base_url = "http://a.test/v1/messages"
            api_key_env = "A_KEY"
        "#;
        assert!(Config::from_toml_str(toml).is_err());
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

    #[test]
    fn provider_auth_style_defaults_to_bearer_and_parses_anthropic() {
        let toml = r#"
            [default]
            provider = "a"
            model = "m"

            [providers.a]
            base_url = "http://a.test/v1/messages"
            api_key_env = "A_KEY"

            [providers.b]
            base_url = "https://api.anthropic.com/v1/messages"
            api_key_env = "B_KEY"
            auth_style = "anthropic"
        "#;
        let cfg = Config::from_toml_str(toml).expect("should parse");
        assert_eq!(cfg.providers["a"].auth_style, AuthStyle::Bearer);
        assert_eq!(cfg.providers["b"].auth_style, AuthStyle::Anthropic);
    }

    #[test]
    fn orchestrator_section_is_optional_and_none_by_default() {
        let toml = r#"
            [default]
            provider = "a"
            model = "m"

            [providers.a]
            base_url = "http://a.test/v1/messages"
            api_key_env = "A_KEY"
        "#;
        let cfg = Config::from_toml_str(toml).expect("should parse");
        assert!(cfg.orchestrator.is_none());
    }

    #[test]
    fn orchestrator_section_parses_with_defaults() {
        let toml = r#"
            [default]
            provider = "qwen"
            model = "qwen3.6:27b"

            [orchestrator]
            local_provider = "qwen"
            escalation_provider = "anthropic"
            escalation_model = "claude-opus-5"

            [providers.qwen]
            base_url = "http://192.168.1.10:8088/v1/messages"
            api_key_env = "LMSTUDIO"

            [providers.anthropic]
            base_url = "https://api.anthropic.com/v1/messages"
            api_key_env = "ANTHROPIC_API_KEY"
            auth_style = "anthropic"
        "#;
        let cfg = Config::from_toml_str(toml).expect("should parse");
        let orch = cfg.orchestrator.expect("section present");
        assert!(orch.enabled);
        assert_eq!(orch.local_provider, "qwen");
        assert_eq!(orch.escalation_provider, "anthropic");
        assert_eq!(orch.escalation_model, "claude-opus-5");
        assert_eq!(orch.sentinel, "<<ESCALATE>>");
        assert_eq!(orch.max_cloud_requests_per_hour, 50);
        assert_eq!(orch.fail_mode, FailMode::Cloud);
    }

    #[test]
    fn orchestrator_overrides_parse() {
        let toml = r#"
            [default]
            provider = "qwen"
            model = "m"

            [orchestrator]
            enabled = false
            local_provider = "qwen"
            escalation_provider = "cloud"
            escalation_model = "big"
            sentinel = "%%UP%%"
            max_cloud_requests_per_hour = 5
            fail_mode = "error"

            [providers.qwen]
            base_url = "http://q.test/v1/messages"
            api_key_env = "Q_KEY"
        "#;
        let orch = Config::from_toml_str(toml).unwrap().orchestrator.unwrap();
        assert!(!orch.enabled);
        assert_eq!(orch.sentinel, "%%UP%%");
        assert_eq!(orch.max_cloud_requests_per_hour, 5);
        assert_eq!(orch.fail_mode, FailMode::Error);
    }
}
