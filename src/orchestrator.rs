//! Escalation state and payload mutation for the hierarchical orchestrator.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::OrchestratorConfig;

/// Which tier owns a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Local,
    Cloud,
}

/// In-memory orchestration state, shared behind an `Arc` in `AppState`.
pub struct Orchestrator {
    pub cfg: OrchestratorConfig,
    sticky: Mutex<HashMap<String, Tier>>,
    cloud_calls: Mutex<VecDeque<Instant>>,
}

impl Orchestrator {
    pub fn new(cfg: OrchestratorConfig) -> Self {
        Orchestrator {
            cfg,
            sticky: Mutex::new(HashMap::new()),
            cloud_calls: Mutex::new(VecDeque::new()),
        }
    }

    pub fn sticky_tier(&self, key: &str) -> Option<Tier> {
        self.sticky.lock().unwrap().get(key).copied()
    }

    pub fn mark_cloud(&self, key: &str) {
        self.sticky
            .lock()
            .unwrap()
            .insert(key.to_string(), Tier::Cloud);
    }

    /// Reserve one cloud call against the sliding hourly budget. Returns false
    /// (reserving nothing) when the cap is reached.
    pub fn try_reserve_cloud_call(&self) -> bool {
        self.try_reserve_cloud_call_at(Instant::now())
    }

    pub fn try_reserve_cloud_call_at(&self, now: Instant) -> bool {
        let mut calls = self.cloud_calls.lock().unwrap();
        let hour = Duration::from_secs(60 * 60);
        while calls
            .front()
            .is_some_and(|t| now.duration_since(*t) >= hour)
        {
            calls.pop_front();
        }
        if (calls.len() as u32) < self.cfg.max_cloud_requests_per_hour {
            calls.push_back(now);
            true
        } else {
            false
        }
    }
}

/// Sticky-conversation key: SHA-256 of the first user message's text content.
/// Claude Code resends the full history each turn, so this is stable for the
/// life of a conversation. Returns `None` when no text-bearing user message
/// exists (callers skip stickiness in that case).
pub fn conversation_key(payload: &Value) -> Option<String> {
    let messages = payload.get("messages")?.as_array()?;
    let first_user = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))?;
    let text = match first_user.get("content")? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n")
        }
        _ => return None,
    };
    Some(format!("{:x}", Sha256::digest(text.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_is_stable_across_turns_of_one_conversation() {
        let turn1 = json!({"messages": [
            {"role": "user", "content": "explain lifetimes"}
        ]});
        let turn2 = json!({"messages": [
            {"role": "user", "content": "explain lifetimes"},
            {"role": "assistant", "content": "They are regions..."},
            {"role": "user", "content": "more detail please"}
        ]});
        let k1 = conversation_key(&turn1).unwrap();
        let k2 = conversation_key(&turn2).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // sha256 hex
    }

    #[test]
    fn different_first_messages_give_different_keys() {
        let a = json!({"messages": [{"role": "user", "content": "alpha"}]});
        let b = json!({"messages": [{"role": "user", "content": "beta"}]});
        assert_ne!(conversation_key(&a), conversation_key(&b));
    }

    #[test]
    fn content_block_arrays_hash_their_text_blocks() {
        let string_form = json!({"messages": [
            {"role": "user", "content": "hello"}
        ]});
        let block_form = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hello"}]}
        ]});
        assert_eq!(
            conversation_key(&string_form),
            conversation_key(&block_form)
        );
    }

    #[test]
    fn missing_user_message_yields_none() {
        assert_eq!(conversation_key(&json!({"messages": []})), None);
        assert_eq!(conversation_key(&json!({})), None);
        let system_only = json!({"messages": [{"role": "assistant", "content": "hi"}]});
        assert_eq!(conversation_key(&system_only), None);
    }

    use crate::config::OrchestratorConfig;
    use std::time::{Duration, Instant};

    fn orch(max_per_hour: u32) -> Orchestrator {
        Orchestrator::new(OrchestratorConfig {
            enabled: true,
            local_provider: "local".into(),
            escalation_provider: "cloud".into(),
            escalation_model: "big".into(),
            sentinel: "<<ESCALATE>>".into(),
            max_cloud_requests_per_hour: max_per_hour,
            fail_mode: crate::config::FailMode::Cloud,
        })
    }

    #[test]
    fn sticky_map_round_trips() {
        let o = orch(10);
        assert_eq!(o.sticky_tier("k1"), None);
        o.mark_cloud("k1");
        assert_eq!(o.sticky_tier("k1"), Some(Tier::Cloud));
        assert_eq!(o.sticky_tier("k2"), None);
    }

    #[test]
    fn budget_allows_up_to_the_cap_within_an_hour() {
        let o = orch(2);
        let t0 = Instant::now();
        assert!(o.try_reserve_cloud_call_at(t0));
        assert!(o.try_reserve_cloud_call_at(t0 + Duration::from_secs(1)));
        assert!(!o.try_reserve_cloud_call_at(t0 + Duration::from_secs(2)));
    }

    #[test]
    fn budget_window_slides() {
        let o = orch(1);
        let t0 = Instant::now();
        assert!(o.try_reserve_cloud_call_at(t0));
        assert!(!o.try_reserve_cloud_call_at(t0 + Duration::from_secs(30 * 60)));
        // The first call ages out after an hour.
        assert!(o.try_reserve_cloud_call_at(t0 + Duration::from_secs(60 * 60 + 1)));
    }

    #[test]
    fn denied_reservation_does_not_consume_budget() {
        let o = orch(1);
        let t0 = Instant::now();
        assert!(o.try_reserve_cloud_call_at(t0));
        assert!(!o.try_reserve_cloud_call_at(t0 + Duration::from_secs(1)));
        // Still exactly one reservation aged out at the hour mark.
        assert!(o.try_reserve_cloud_call_at(t0 + Duration::from_secs(60 * 60 + 1)));
    }
}
