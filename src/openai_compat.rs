//! OpenAI chat dialect <-> Anthropic Messages dialect translation.
//!
//! Inbound: what Open WebUI sends to `/v1/chat/completions` becomes an
//! Anthropic Messages payload that enters the normal pipeline. Outbound:
//! Anthropic responses (JSON and SSE) become OpenAI responses. Unsupported
//! OpenAI fields (tools, n, penalties, ...) are dropped; multimodal content
//! is out of scope per the spec.

use serde_json::{json, Value};

/// Flatten an OpenAI message `content` (string, or array of parts) to text.
fn content_text(content: Option<&Value>) -> Result<String, String> {
    match content {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            if texts.is_empty() {
                return Err("message content has no text parts".into());
            }
            Ok(texts.join("\n"))
        }
        _ => Err("message missing 'content'".into()),
    }
}

/// Translate an OpenAI chat-completions request into an Anthropic Messages
/// payload. `model` is left empty -- routing decides it.
pub fn openai_to_anthropic(req: &Value) -> Result<Value, String> {
    let messages = req
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing 'messages' array".to_string())?;

    let mut system_parts: Vec<String> = Vec::new();
    let mut out_messages: Vec<Value> = Vec::new();
    for m in messages {
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "message missing 'role'".to_string())?;
        let text = content_text(m.get("content"))?;
        match role {
            // "developer" is OpenAI's newer name for the system role.
            "system" | "developer" => system_parts.push(text),
            "user" | "assistant" => out_messages.push(json!({"role": role, "content": text})),
            other => return Err(format!("unsupported role '{other}'")),
        }
    }
    if out_messages.is_empty() {
        return Err("no user or assistant messages".into());
    }

    let max_tokens = req
        .get("max_tokens")
        .or_else(|| req.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(4096);

    let mut out = json!({
        "model": "",
        "max_tokens": max_tokens,
        "messages": out_messages,
    });
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n\n"));
    }
    for key in ["temperature", "top_p"] {
        if let Some(v) = req.get(key) {
            out[key] = v.clone();
        }
    }
    match req.get("stop") {
        Some(Value::String(s)) => out["stop_sequences"] = json!([s]),
        Some(Value::Array(a)) => out["stop_sequences"] = Value::Array(a.clone()),
        _ => {}
    }
    if req.get("stream").and_then(Value::as_bool) == Some(true) {
        out["stream"] = json!(true);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_translates_with_default_max_tokens() {
        let req = json!({
            "model": "big-brother",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = openai_to_anthropic(&req).unwrap();
        assert_eq!(out["model"], "");
        assert_eq!(out["max_tokens"], 4096);
        assert_eq!(out["messages"], json!([{"role": "user", "content": "hi"}]));
        assert!(out.get("system").is_none());
        assert!(out.get("stream").is_none());
    }

    #[test]
    fn system_messages_merge_into_system_field() {
        let req = json!({"messages": [
            {"role": "system", "content": "Be terse."},
            {"role": "developer", "content": "Answer in French."},
            {"role": "user", "content": "hi"}
        ]});
        let out = openai_to_anthropic(&req).unwrap();
        assert_eq!(out["system"], "Be terse.\n\nAnswer in French.");
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sampling_params_and_stream_carry_over() {
        let req = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.2, "top_p": 0.9, "max_tokens": 128,
            "stop": "END", "stream": true
        });
        let out = openai_to_anthropic(&req).unwrap();
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["top_p"], 0.9);
        assert_eq!(out["max_tokens"], 128);
        assert_eq!(out["stop_sequences"], json!(["END"]));
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn array_content_parts_flatten_to_text() {
        let req = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "line one"},
            {"type": "text", "text": "line two"}
        ]}]});
        let out = openai_to_anthropic(&req).unwrap();
        assert_eq!(out["messages"][0]["content"], "line one\nline two");
    }

    #[test]
    fn bad_requests_are_rejected_with_reason() {
        assert!(openai_to_anthropic(&json!({}))
            .unwrap_err()
            .contains("messages"));
        let no_text = json!({"messages": [{"role": "user", "content": [{"type": "image_url"}]}]});
        assert!(openai_to_anthropic(&no_text).is_err());
        let tool_role = json!({"messages": [{"role": "tool", "content": "x"}]});
        assert!(openai_to_anthropic(&tool_role).unwrap_err().contains("tool"));
        let system_only = json!({"messages": [{"role": "system", "content": "x"}]});
        assert!(openai_to_anthropic(&system_only).is_err());
    }
}
