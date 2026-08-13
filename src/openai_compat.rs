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

/// Incremental Anthropic-SSE -> OpenAI-SSE translator.
///
/// Fed raw upstream bytes; emits OpenAI `chat.completion.chunk` events plus a
/// final `data: [DONE]`. Mirrors `stream::SseTextScanner`'s line-buffering so
/// events split across network chunks reassemble correctly. Non-`data:` lines
/// and unparseable JSON are dropped (OpenAI clients only need chunks).
pub struct SseTranslator {
    pending: String,
    id: String,
    model: String,
    created: u64,
    done_sent: bool,
}

impl SseTranslator {
    pub fn new() -> Self {
        SseTranslator {
            pending: String::new(),
            id: "chatcmpl-big-brother".to_string(),
            model: "big-brother".to_string(),
            created: epoch_secs(),
            done_sent: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&'static str>) -> Vec<u8> {
        let event = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {event}\n\n").into_bytes()
    }

    /// Feed one raw upstream chunk; returns translated bytes (possibly empty).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                        self.id = id.to_string();
                    }
                    if let Some(m) = event.pointer("/message/model").and_then(Value::as_str) {
                        self.model = m.to_string();
                    }
                    out.extend(self.chunk(json!({"role": "assistant", "content": ""}), None));
                }
                Some("content_block_start") => {
                    if let Some(t) = event.pointer("/content_block/text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            out.extend(self.chunk(json!({"content": t}), None));
                        }
                    }
                }
                Some("content_block_delta") => {
                    if let Some(t) = event.pointer("/delta/text").and_then(Value::as_str) {
                        out.extend(self.chunk(json!({"content": t}), None));
                    }
                }
                Some("message_delta") => {
                    let reason =
                        finish_reason(event.pointer("/delta/stop_reason").and_then(Value::as_str));
                    out.extend(self.chunk(json!({}), Some(reason)));
                }
                Some("message_stop") => {
                    if !self.done_sent {
                        out.extend_from_slice(b"data: [DONE]\n\n");
                        self.done_sent = true;
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Terminate the OpenAI stream if the upstream ended without message_stop.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.done_sent {
            return Vec::new();
        }
        self.done_sent = true;
        b"data: [DONE]\n\n".to_vec()
    }
}

impl Default for SseTranslator {
    fn default() -> Self {
        Self::new()
    }
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

    fn sse(event: &Value) -> String {
        format!("data: {event}\n\n")
    }

    /// Collect the `data:` payloads (and [DONE] markers) from translator output.
    fn out_events(bytes: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|l| l.strip_prefix("data: ").map(str::to_string))
            .collect()
    }

    #[test]
    fn sse_stream_translates_to_openai_chunks() {
        let mut t = SseTranslator::new();
        let mut all = Vec::new();
        all.extend(t.push(
            sse(&json!({"type": "message_start",
                "message": {"id": "msg_9", "model": "qwen3.6:27b"}}))
            .as_bytes(),
        ));
        all.extend(t.push(
            sse(&json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}))
            .as_bytes(),
        ));
        all.extend(t.push(
            sse(&json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "Hi"}}))
            .as_bytes(),
        ));
        all.extend(t.push(
            sse(&json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}))
            .as_bytes(),
        ));
        all.extend(t.push(sse(&json!({"type": "message_stop"})).as_bytes()));

        let events = out_events(&all);
        assert_eq!(events.last().unwrap(), "[DONE]");
        let chunks: Vec<Value> = events[..events.len() - 1]
            .iter()
            .map(|e| serde_json::from_str(e).unwrap())
            .collect();
        // Role chunk, content chunk, finish chunk.
        assert_eq!(chunks[0]["object"], "chat.completion.chunk");
        assert_eq!(chunks[0]["id"], "msg_9");
        assert_eq!(chunks[0]["model"], "qwen3.6:27b");
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hi");
        assert!(chunks[1]["choices"][0]["finish_reason"].is_null());
        assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn sse_translator_handles_chunks_split_mid_line() {
        let mut t = SseTranslator::new();
        let line = sse(&json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}}));
        let bytes = line.as_bytes();
        let mut all = t.push(&bytes[..7]);
        all.extend(t.push(&bytes[7..]));
        let events = out_events(&all);
        assert_eq!(events.len(), 1);
        let chunk: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["content"], "Hello");
    }

    #[test]
    fn finish_emits_done_exactly_once() {
        let mut t = SseTranslator::new();
        t.push(sse(&json!({"type": "message_stop"})).as_bytes());
        assert!(t.finish().is_empty()); // [DONE] already sent by message_stop
        let mut t2 = SseTranslator::new();
        assert_eq!(out_events(&t2.finish()), vec!["[DONE]"]); // abrupt end
        assert!(t2.finish().is_empty());
    }

    #[test]
    fn non_data_lines_and_bad_json_are_ignored() {
        let mut t = SseTranslator::new();
        let noise = "event: message_start\ndata: {not json}\n\n";
        assert!(t.push(noise.as_bytes()).is_empty());
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
