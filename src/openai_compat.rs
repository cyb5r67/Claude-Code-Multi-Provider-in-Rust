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

/// Unix epoch seconds for OpenAI `created` fields.
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Map an Anthropic stop_reason to the OpenAI finish_reason vocabulary.
pub fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// Translate a non-streaming Anthropic Messages response into an OpenAI
/// chat.completion object.
pub fn anthropic_to_openai(resp: &Value) -> Value {
    let text: String = resp
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let prompt = resp
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = resp
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "id": resp.get("id").and_then(Value::as_str).unwrap_or("chatcmpl-big-brother"),
        "object": "chat.completion",
        "created": epoch_secs(),
        "model": resp.get("model").and_then(Value::as_str).unwrap_or("big-brother"),
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": finish_reason(resp.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    })
}

/// OpenAI-shaped error body; every chat-route failure uses this shape.
pub fn openai_error_body(message: &str, r#type: &str, code: u16) -> Value {
    json!({"error": {"message": message, "type": r#type, "code": code}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_translates_to_chat_completion() {
        let resp = json!({
            "id": "msg_01", "model": "qwen3.6:27b", "stop_reason": "end_turn",
            "content": [
                {"type": "text", "text": "Hello "},
                {"type": "text", "text": "world"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let out = anthropic_to_openai(&resp);
        assert_eq!(out["id"], "msg_01");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "qwen3.6:27b");
        assert!(out["created"].as_u64().unwrap() > 1_700_000_000);
        assert_eq!(out["choices"][0]["index"], 0);
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            out["usage"],
            json!({
                "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15
            })
        );
    }

    #[test]
    fn finish_reasons_map() {
        assert_eq!(finish_reason(Some("end_turn")), "stop");
        assert_eq!(finish_reason(Some("stop_sequence")), "stop");
        assert_eq!(finish_reason(Some("max_tokens")), "length");
        assert_eq!(finish_reason(Some("tool_use")), "tool_calls");
        assert_eq!(finish_reason(Some("anything_else")), "stop");
        assert_eq!(finish_reason(None), "stop");
    }

    #[test]
    fn error_body_is_openai_shaped() {
        let body = openai_error_body("boom", "invalid_request_error", 400);
        assert_eq!(body["error"]["message"], "boom");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], 400);
    }

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
