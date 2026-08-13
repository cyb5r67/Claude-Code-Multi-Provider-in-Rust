//! Runtime chat settings: panel-editable, persisted to a JSON state file.
//!
//! `config.toml`'s `[chat]` section provides the defaults; the state file
//! (written on every panel edit) wins when present so choices survive
//! restarts. A corrupt or unreadable state file falls back to the config
//! defaults with a warning -- it never fails startup.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::config::ChatConfig;

/// The panel-editable subset of chat behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSettings {
    pub pipeline_enabled: bool,
    /// "cascade" or an explicit "provider/model" routing target.
    pub model_override: String,
}

/// Shared runtime state: current settings plus where to persist them.
pub struct ChatState {
    settings: RwLock<ChatSettings>,
    path: PathBuf,
}

impl ChatState {
    /// Build initial state: the state file wins over config defaults.
    pub fn load(cfg: &ChatConfig) -> ChatState {
        let path = PathBuf::from(&cfg.state_file);
        let defaults = ChatSettings {
            pipeline_enabled: cfg.pipeline_enabled,
            model_override: cfg.model_override.clone(),
        };
        let settings = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<ChatSettings>(&text) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "chat state file is corrupt; using config defaults");
                    defaults
                }
            },
            Err(_) => defaults, // absent file is the normal first-run case
        };
        ChatState {
            settings: RwLock::new(settings),
            path,
        }
    }

    pub fn get(&self) -> ChatSettings {
        self.settings.read().unwrap().clone()
    }

    /// Update in memory and persist. The in-memory update happens even if the
    /// write fails (the caller reports the error; behavior stays consistent
    /// until restart).
    pub fn set(&self, new: ChatSettings) -> std::io::Result<()> {
        *self.settings.write().unwrap() = new.clone();
        let text = serde_json::to_string_pretty(&new).expect("settings serialize");
        std::fs::write(&self.path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(state_file: &str) -> ChatConfig {
        ChatConfig {
            pipeline_enabled: true,
            model_override: "cascade".into(),
            passthrough_url: "http://lan.test/v1/chat/completions".into(),
            passthrough_model: "q".into(),
            state_file: state_file.into(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bb_chat_settings_{name}_{}", std::process::id()));
        p
    }

    #[test]
    fn missing_state_file_uses_config_defaults() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let state = ChatState::load(&cfg(path.to_str().unwrap()));
        assert_eq!(
            state.get(),
            ChatSettings {
                pipeline_enabled: true,
                model_override: "cascade".into()
            }
        );
    }

    #[test]
    fn set_persists_and_load_reads_it_back() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let c = cfg(path.to_str().unwrap());
        let state = ChatState::load(&c);
        let new = ChatSettings {
            pipeline_enabled: false,
            model_override: "a/m".into(),
        };
        state.set(new.clone()).expect("write state file");
        // A fresh load (simulated restart) sees the persisted values.
        let reloaded = ChatState::load(&c);
        assert_eq!(reloaded.get(), new);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_state_file_falls_back_to_defaults() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{not json").unwrap();
        let state = ChatState::load(&cfg(path.to_str().unwrap()));
        assert_eq!(state.get().model_override, "cascade");
        let _ = std::fs::remove_file(&path);
    }
}
