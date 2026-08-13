//! Escalation state and payload mutation for the hierarchical orchestrator.

use serde_json::Value;
use sha2::{Digest, Sha256};

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
}
