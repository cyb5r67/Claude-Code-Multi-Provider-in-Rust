//! Escalation state and payload mutation for the hierarchical orchestrator.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{FailMode, OrchestratorConfig};

/// Which tier owns a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Local,
    Cloud,
}

/// Maximum escalation records retained in memory (oldest evicted first).
pub const ESCALATION_HISTORY_CAP: usize = 500;

/// One recorded escalation (or budget-denied fallback) for the status panel.
/// Never contains message content — conversations appear only as a key prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EscalationRecord {
    /// Unix epoch seconds; the panel renders local time in the browser.
    pub at_epoch_secs: u64,
    pub trigger: String,
    pub provider: String,
    pub model: String,
    pub conversation_key_prefix: Option<String>,
}

/// Ring buffer plus running totals (totals survive eviction).
#[derive(Default)]
struct History {
    records: VecDeque<EscalationRecord>,
    total_escalations: u64,
    total_budget_denied: u64,
}

/// Serializable snapshot for `GET /status`.
#[derive(Debug, Clone, Serialize)]
pub struct OrchestratorStatus {
    pub enabled: bool,
    pub local_provider: String,
    pub escalation_provider: String,
    pub escalation_model: String,
    pub sentinel: String,
    pub fail_mode: FailMode,
    pub budget: BudgetStatus,
    pub sticky_cloud_conversations: usize,
    pub escalations: EscalationsStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetStatus {
    pub max_per_hour: u32,
    pub used_last_hour: u32,
    pub remaining: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EscalationsStatus {
    pub total_since_start: u64,
    pub budget_denied_since_start: u64,
    /// Newest first, at most `ESCALATION_HISTORY_CAP` entries.
    pub recent: Vec<EscalationRecord>,
}

/// Drop reservations older than the sliding one-hour window.
fn prune_window(calls: &mut VecDeque<Instant>, now: Instant) {
    let hour = Duration::from_secs(60 * 60);
    while calls
        .front()
        .is_some_and(|t| now.duration_since(*t) >= hour)
    {
        calls.pop_front();
    }
}

/// In-memory orchestration state, shared behind an `Arc` in `AppState`.
pub struct Orchestrator {
    pub cfg: OrchestratorConfig,
    sticky: Mutex<HashMap<String, Tier>>,
    cloud_calls: Mutex<VecDeque<Instant>>,
    history: Mutex<History>,
}

impl Orchestrator {
    pub fn new(cfg: OrchestratorConfig) -> Self {
        Orchestrator {
            cfg,
            sticky: Mutex::new(HashMap::new()),
            cloud_calls: Mutex::new(VecDeque::new()),
            history: Mutex::new(History::default()),
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
        prune_window(&mut calls, now);
        if (calls.len() as u32) < self.cfg.max_cloud_requests_per_hour {
            calls.push_back(now);
            true
        } else {
            false
        }
    }

    /// Record an escalation event for the status panel. `trigger` is one of
    /// `sentinel`/`sticky`/`fail_mode` (granted escalations) or
    /// `budget_denied` (local fallback).
    pub fn record_escalation(&self, trigger: &str, provider: &str, model: &str, key: Option<&str>) {
        let at_epoch_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let record = EscalationRecord {
            at_epoch_secs,
            trigger: trigger.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            conversation_key_prefix: key.map(|k| k.chars().take(8).collect()),
        };
        let mut history = self.history.lock().unwrap();
        if trigger == "budget_denied" {
            history.total_budget_denied += 1;
        } else {
            history.total_escalations += 1;
        }
        if history.records.len() == ESCALATION_HISTORY_CAP {
            history.records.pop_front();
        }
        history.records.push_back(record);
    }

    /// Reservations currently inside the sliding hour. Read-only: never consumes budget.
    pub fn budget_used(&self) -> u32 {
        self.budget_used_at(Instant::now())
    }

    pub fn budget_used_at(&self, now: Instant) -> u32 {
        let mut calls = self.cloud_calls.lock().unwrap();
        prune_window(&mut calls, now);
        calls.len() as u32
    }

    pub fn sticky_count(&self) -> usize {
        self.sticky.lock().unwrap().len()
    }

    /// One serializable snapshot; the HTTP handler never touches internals.
    pub fn status(&self) -> OrchestratorStatus {
        let used = self.budget_used();
        let max = self.cfg.max_cloud_requests_per_hour;
        // Take each lock in its own scope: acquiring the sticky lock while the
        // history guard is alive would create an undocumented lock-order
        // invariant a future caller could deadlock against.
        let sticky_cloud_conversations = self.sticky_count();
        let history = self.history.lock().unwrap();
        OrchestratorStatus {
            enabled: self.cfg.enabled,
            local_provider: self.cfg.local_provider.clone(),
            escalation_provider: self.cfg.escalation_provider.clone(),
            escalation_model: self.cfg.escalation_model.clone(),
            sentinel: self.cfg.sentinel.clone(),
            fail_mode: self.cfg.fail_mode,
            budget: BudgetStatus {
                max_per_hour: max,
                used_last_hour: used,
                remaining: max.saturating_sub(used),
            },
            sticky_cloud_conversations,
            escalations: EscalationsStatus {
                total_since_start: history.total_escalations,
                budget_denied_since_start: history.total_budget_denied,
                recent: history.records.iter().rev().cloned().collect(),
            },
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

/// System note appended when the budget denies an escalation Qwen asked for.
pub const ESCALATION_UNAVAILABLE_NOTE: &str =
    "Escalation is currently unavailable; answer the request yourself as best you can.";

/// The instruction injected into local-tier attempts (spec wording).
pub fn sentinel_instruction(sentinel: &str) -> String {
    format!(
        "If this task is beyond your capability, output {sentinel} as your \
         very first token and nothing else."
    )
}

/// Append a note to the request's system prompt, whatever shape it has.
pub fn append_system_note(payload: &mut Value, note: &str) {
    match payload.get_mut("system") {
        Some(Value::String(s)) => {
            s.push_str("\n\n");
            s.push_str(note);
        }
        Some(Value::Array(blocks)) => {
            blocks.push(serde_json::json!({"type": "text", "text": note}));
        }
        _ => {
            payload["system"] = Value::String(note.to_string());
        }
    }
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

    #[test]
    fn note_appends_to_string_system() {
        let mut payload = json!({"system": "Be terse.", "messages": []});
        append_system_note(&mut payload, "NOTE");
        assert_eq!(payload["system"], "Be terse.\n\nNOTE");
    }

    #[test]
    fn note_appends_block_to_array_system() {
        let mut payload = json!({
            "system": [{"type": "text", "text": "Be terse."}],
            "messages": []
        });
        append_system_note(&mut payload, "NOTE");
        assert_eq!(
            payload["system"],
            json!([
                {"type": "text", "text": "Be terse."},
                {"type": "text", "text": "NOTE"}
            ])
        );
    }

    #[test]
    fn note_creates_system_when_absent() {
        let mut payload = json!({"messages": []});
        append_system_note(&mut payload, "NOTE");
        assert_eq!(payload["system"], "NOTE");
    }

    #[test]
    fn sentinel_instruction_names_the_sentinel() {
        let text = sentinel_instruction("<<ESCALATE>>");
        assert!(text.contains("<<ESCALATE>>"));
        assert!(text.contains("very first token"));
    }

    #[test]
    fn history_caps_at_500_and_totals_survive_eviction() {
        let o = orch(10);
        for i in 0..502 {
            o.record_escalation("sentinel", "cloud", &format!("m{i}"), None);
        }
        o.record_escalation("budget_denied", "local", "lm", None);
        let s = o.status();
        assert_eq!(s.escalations.recent.len(), ESCALATION_HISTORY_CAP);
        assert_eq!(s.escalations.total_since_start, 502);
        assert_eq!(s.escalations.budget_denied_since_start, 1);
        // Newest first: 503 records total, cap 500, so m0..m2 were evicted.
        assert_eq!(s.escalations.recent[0].trigger, "budget_denied");
        assert_eq!(s.escalations.recent[1].model, "m501");
        assert_eq!(s.escalations.recent.last().unwrap().model, "m3");
    }

    #[test]
    fn key_prefix_is_first_8_chars() {
        let o = orch(10);
        o.record_escalation("sticky", "cloud", "m", Some("abcdef0123456789"));
        o.record_escalation("sticky", "cloud", "m", None);
        let s = o.status();
        assert_eq!(
            s.escalations.recent[1].conversation_key_prefix.as_deref(),
            Some("abcdef01")
        );
        assert_eq!(s.escalations.recent[0].conversation_key_prefix, None);
    }

    #[test]
    fn budget_used_counts_without_consuming_and_prunes() {
        let o = orch(2);
        let t0 = Instant::now();
        assert_eq!(o.budget_used_at(t0), 0);
        assert!(o.try_reserve_cloud_call_at(t0));
        assert_eq!(o.budget_used_at(t0 + Duration::from_secs(1)), 1);
        // Reading twice does not consume budget.
        assert_eq!(o.budget_used_at(t0 + Duration::from_secs(2)), 1);
        assert!(o.try_reserve_cloud_call_at(t0 + Duration::from_secs(3)));
        assert_eq!(o.budget_used_at(t0 + Duration::from_secs(4)), 2);
        // The first reservation ages out of the sliding hour.
        assert_eq!(o.budget_used_at(t0 + Duration::from_secs(60 * 60 + 1)), 1);
    }

    #[test]
    fn status_snapshot_reflects_config_budget_and_sticky() {
        let o = orch(5);
        o.mark_cloud("k1");
        o.mark_cloud("k2");
        assert!(o.try_reserve_cloud_call());
        let s = o.status();
        assert!(s.enabled);
        assert_eq!(s.local_provider, "local");
        assert_eq!(s.escalation_provider, "cloud");
        assert_eq!(s.escalation_model, "big");
        assert_eq!(s.sentinel, "<<ESCALATE>>");
        assert_eq!(s.budget.max_per_hour, 5);
        assert_eq!(s.budget.used_last_hour, 1);
        assert_eq!(s.budget.remaining, 4);
        assert_eq!(s.sticky_cloud_conversations, 2);
    }

    #[test]
    fn status_serializes_with_lowercase_enums_and_epoch_timestamps() {
        let o = orch(5);
        o.record_escalation("sentinel", "cloud", "big", Some("aabbccddeeff0011"));
        let v = serde_json::to_value(o.status()).unwrap();
        assert_eq!(v["fail_mode"], "cloud");
        assert_eq!(
            v["escalations"]["recent"][0]["conversation_key_prefix"],
            "aabbccdd"
        );
        let at = v["escalations"]["recent"][0]["at_epoch_secs"]
            .as_u64()
            .unwrap();
        assert!(
            at > 1_700_000_000,
            "expected a current epoch timestamp, got {at}"
        );
    }
}
