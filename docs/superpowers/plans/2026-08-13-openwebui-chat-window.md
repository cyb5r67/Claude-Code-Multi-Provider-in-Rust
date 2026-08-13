# Open WebUI Chat Window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Open WebUI chat through Big Brother's routing/cascade/escalation pipeline, with a panel toggle (pipeline on/off) and routing-target picker, persisted across restarts.

**Architecture:** Big Brother gains OpenAI-dialect endpoints (`POST /v1/chat/completions`, `GET /v1/models`). A translator module converts OpenAI⇄Anthropic in both directions (streaming included); translated requests enter the existing cascade/forward pipeline unchanged. Runtime chat settings live in `AppState`, are edited via `GET/PUT /chat/settings` from the panel, and persist to a JSON state file.

**Tech Stack:** Rust 2021, axum 0.7, reqwest 0.12, serde_json, tokio; tests with `cargo test` (unit + wiremock/tower integration).

**Spec:** `docs/superpowers/specs/2026-08-13-openwebui-chat-window-design.md`

## Global Constraints

- All errors returned on the chat routes are OpenAI-shaped: `{"error": {"message": ..., "type": ..., "code": ...}}`.
- `GET /v1/models` advertises exactly one virtual model id: `big-brother`.
- The `[chat]` config section is optional; when absent, chat routes return 404 OpenAI-shaped errors and the panel hides its Chat card. Nothing else changes.
- Corrupt or missing state file must never fail startup — fall back to `[chat]` config defaults with a `tracing::warn!`.
- Cloud budget is shared with Claude Code traffic (use the existing `Orchestrator`, no second budget).
- Passthrough mode rewrites only the `model` field; everything else forwards verbatim.
- Follow existing code style: module-level `//!` docs, `///` on public items, tests in `#[cfg(test)] mod tests` (unit) or `tests/` (integration), unique env-var names per test.
- Run `cargo test` after every implementation step; commit at the end of every task.

---

### Task 1: `[chat]` config section

**Files:**
- Modify: `src/config.rs` (add `ChatConfig`, field on `Config`, defaults, tests)

**Interfaces:**
- Produces: `config::ChatConfig { pipeline_enabled: bool, model_override: String, passthrough_url: String, passthrough_model: String, state_file: String }`; `Config.chat: Option<ChatConfig>`.

- [ ] **Step 1: Write failing tests** — append to `mod tests` in `src/config.rs`:

```rust
#[test]
fn chat_section_is_optional_and_none_by_default() {
    let toml = r#"
        [default]
        provider = "a"
        model = "m"

        [providers.a]
        base_url = "http://a.test/v1/messages"
        api_key_env = "A_KEY"
    "#;
    let cfg = Config::from_toml_str(toml).expect("should parse");
    assert!(cfg.chat.is_none());
}

#[test]
fn chat_section_parses_with_defaults() {
    let toml = r#"
        [default]
        provider = "a"
        model = "m"

        [chat]
        passthrough_url = "http://lan.test:8088/v1/chat/completions"
        passthrough_model = "qwen3.6:27b"

        [providers.a]
        base_url = "http://a.test/v1/messages"
        api_key_env = "A_KEY"
    "#;
    let chat = Config::from_toml_str(toml).unwrap().chat.expect("section present");
    assert!(chat.pipeline_enabled);
    assert_eq!(chat.model_override, "cascade");
    assert_eq!(chat.passthrough_url, "http://lan.test:8088/v1/chat/completions");
    assert_eq!(chat.passthrough_model, "qwen3.6:27b");
    assert_eq!(chat.state_file, "chat_state.json");
}

#[test]
fn chat_overrides_parse() {
    let toml = r#"
        [default]
        provider = "a"
        model = "m"

        [chat]
        pipeline_enabled = false
        model_override = "a/m2"
        passthrough_url = "http://lan.test:8088/v1/chat/completions"
        passthrough_model = "q"
        state_file = "/data/chat_state.json"

        [providers.a]
        base_url = "http://a.test/v1/messages"
        api_key_env = "A_KEY"
    "#;
    let chat = Config::from_toml_str(toml).unwrap().chat.unwrap();
    assert!(!chat.pipeline_enabled);
    assert_eq!(chat.model_override, "a/m2");
    assert_eq!(chat.state_file, "/data/chat_state.json");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --lib config` — expected: compile error, `Config` has no field `chat`.

- [ ] **Step 3: Implement** — in `src/config.rs`, add to `Config`:

```rust
    #[serde(default)]
    pub chat: Option<ChatConfig>,
```

and below `OrchestratorConfig`:

```rust
/// Settings for the OpenAI-dialect chat endpoint (Open WebUI front door).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatConfig {
    /// Route chat traffic through the routing/cascade pipeline (true) or pass
    /// it straight through to the local OpenAI-dialect endpoint (false).
    #[serde(default = "default_true")]
    pub pipeline_enabled: bool,
    /// "cascade" or an explicit "provider/model" routing target.
    #[serde(default = "default_chat_target")]
    pub model_override: String,
    /// OpenAI-dialect endpoint used in passthrough mode (LM Studio's
    /// /v1/chat/completions -- NOT the Anthropic-dialect provider base_url).
    pub passthrough_url: String,
    /// Model id written into passthrough requests (the client sends the
    /// virtual "big-brother" id, meaningless upstream).
    pub passthrough_model: String,
    /// Where panel edits are persisted. Relative paths resolve against the
    /// process working directory.
    #[serde(default = "default_chat_state_file")]
    pub state_file: String,
}

fn default_chat_target() -> String {
    "cascade".to_string()
}
fn default_chat_state_file() -> String {
    "chat_state.json".to_string()
}
```

- [ ] **Step 4: Verify pass** — `cargo test --lib config` — expected: all pass.
- [ ] **Step 5: Commit** — `git add src/config.rs && git commit -m "feat: add [chat] config section"`

---

### Task 2: Chat settings state with persistence

**Files:**
- Create: `src/chat_settings.rs`
- Modify: `src/lib.rs` (add `pub mod chat_settings;`)

**Interfaces:**
- Consumes: `config::ChatConfig` (Task 1).
- Produces: `chat_settings::ChatSettings { pipeline_enabled: bool, model_override: String }` (Clone, PartialEq, Serialize, Deserialize); `chat_settings::ChatState` with `pub fn load(cfg: &ChatConfig) -> ChatState`, `pub fn get(&self) -> ChatSettings`, `pub fn set(&self, s: ChatSettings) -> std::io::Result<()>`.

- [ ] **Step 1: Write the module with failing tests** — create `src/chat_settings.rs`:

```rust
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
            ChatSettings { pipeline_enabled: true, model_override: "cascade".into() }
        );
    }

    #[test]
    fn set_persists_and_load_reads_it_back() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let c = cfg(path.to_str().unwrap());
        let state = ChatState::load(&c);
        let new = ChatSettings { pipeline_enabled: false, model_override: "a/m".into() };
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
```

- [ ] **Step 2: Register the module** — in `src/lib.rs` add `pub mod chat_settings;` after `pub mod config;` (alphabetical order with the others).
- [ ] **Step 3: Run tests** — `cargo test --lib chat_settings` — expected: PASS (module + tests land together; the failing state here is the pre-edit compile).
- [ ] **Step 4: Add `chat_state.json` to `.gitignore`** — append a line `chat_state.json` (create `.gitignore` entry if the file lacks it).
- [ ] **Step 5: Commit** — `git add src/chat_settings.rs src/lib.rs .gitignore && git commit -m "feat: add persisted chat settings state"`

---

### Task 3: OpenAI→Anthropic request translation

**Files:**
- Create: `src/openai_compat.rs`
- Modify: `src/lib.rs` (add `pub mod openai_compat;`)

**Interfaces:**
- Produces: `openai_compat::openai_to_anthropic(req: &Value) -> Result<Value, String>` (Err = human-readable 400 message). The returned payload has `model: ""` — dispatch fills it.

- [ ] **Step 1: Create the module with tests** — `src/openai_compat.rs`:

```rust
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
            "user" | "assistant" => {
                out_messages.push(json!({"role": role, "content": text}))
            }
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
        assert!(openai_to_anthropic(&json!({})).unwrap_err().contains("messages"));
        let no_text = json!({"messages": [{"role": "user", "content": [{"type": "image_url"}]}]});
        assert!(openai_to_anthropic(&no_text).is_err());
        let tool_role = json!({"messages": [{"role": "tool", "content": "x"}]});
        assert!(openai_to_anthropic(&tool_role).unwrap_err().contains("tool"));
        let system_only = json!({"messages": [{"role": "system", "content": "x"}]});
        assert!(openai_to_anthropic(&system_only).is_err());
    }
}
```

- [ ] **Step 2: Register** — add `pub mod openai_compat;` to `src/lib.rs`.
- [ ] **Step 3: Run** — `cargo test --lib openai_compat` — expected: PASS.
- [ ] **Step 4: Commit** — `git add src/openai_compat.rs src/lib.rs && git commit -m "feat: translate OpenAI chat requests to Anthropic Messages"`

---

### Task 4: Anthropic→OpenAI response translation (non-streaming + errors)

**Files:**
- Modify: `src/openai_compat.rs`

**Interfaces:**
- Produces: `openai_compat::anthropic_to_openai(resp: &Value) -> Value`; `openai_compat::finish_reason(stop_reason: Option<&str>) -> &'static str`; `openai_compat::openai_error_body(message: &str, r#type: &str, code: u16) -> Value`; `openai_compat::epoch_secs() -> u64`.

- [ ] **Step 1: Write failing tests** — append to `mod tests`:

```rust
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
    assert_eq!(out["usage"], json!({
        "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15
    }));
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
```

- [ ] **Step 2: Verify failure** — `cargo test --lib openai_compat` — expected: compile error, functions not defined.

- [ ] **Step 3: Implement** — add to `src/openai_compat.rs`:

```rust
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
    let prompt = resp.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion = resp.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0);
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
```

- [ ] **Step 4: Verify pass** — `cargo test --lib openai_compat` — expected: PASS.
- [ ] **Step 5: Commit** — `git add src/openai_compat.rs && git commit -m "feat: translate Anthropic responses to OpenAI chat.completion"`

---

### Task 5: Streaming SSE translator

**Files:**
- Modify: `src/openai_compat.rs`

**Interfaces:**
- Produces: `openai_compat::SseTranslator` with `pub fn new() -> Self`, `pub fn push(&mut self, chunk: &[u8]) -> Vec<u8>` (returns OpenAI-formatted SSE bytes, possibly empty), `pub fn finish(&mut self) -> Vec<u8>` (emits `data: [DONE]` if not already sent).

- [ ] **Step 1: Write failing tests** — append to `mod tests`:

```rust
fn sse(event: &Value) -> String {
    format!("data: {event}\n\n")
}

/// Collect the `data:` JSON payloads (and [DONE] markers) from translator output.
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
    all.extend(t.push(sse(&json!({"type": "message_start",
        "message": {"id": "msg_9", "model": "qwen3.6:27b"}})).as_bytes()));
    all.extend(t.push(sse(&json!({"type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}})).as_bytes()));
    all.extend(t.push(sse(&json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "Hi"}})).as_bytes()));
    all.extend(t.push(sse(&json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}})).as_bytes()));
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
```

- [ ] **Step 2: Verify failure** — `cargo test --lib openai_compat` — expected: compile error, `SseTranslator` not defined.

- [ ] **Step 3: Implement** — add to `src/openai_compat.rs`:

```rust
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
            let Some(data) = line.strip_prefix("data: ") else { continue };
            let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
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
```

- [ ] **Step 4: Verify pass** — `cargo test --lib openai_compat` — expected: PASS.
- [ ] **Step 5: Commit** — `git add src/openai_compat.rs && git commit -m "feat: streaming Anthropic-to-OpenAI SSE translator"`

---

### Task 6: Wire chat state into AppState, add `/v1/models` and `/chat/settings`

**Files:**
- Create: `src/chat_proxy.rs` (handlers)
- Modify: `src/lib.rs` (module + `build_state`), `src/proxy.rs` (`AppState` field + router entries)
- Test: `tests/chat_integration.rs` (new)

**Interfaces:**
- Consumes: `chat_settings::ChatState` (Task 2), `config::ChatConfig` (Task 1), `openai_compat::openai_error_body` (Task 4).
- Produces: `AppState.chat: Option<Arc<chat_settings::ChatState>>`; routes `GET /v1/models`, `GET /chat/settings`, `PUT /chat/settings`; handlers `chat_proxy::models`, `chat_proxy::get_settings`, `chat_proxy::put_settings`. `GET /chat/settings` returns `{"pipeline_enabled", "model_override", "targets": ["cascade", "<provider>/<model>", ...]}` where targets lists every provider with a configured `model`.

- [ ] **Step 1: Create `src/chat_proxy.rs`** with the non-completions handlers:

```rust
//! OpenAI-dialect chat routes: /v1/models, /v1/chat/completions, and the
//! panel's /chat/settings editor.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::chat_settings::ChatSettings;
use crate::openai_compat::openai_error_body;
use crate::proxy::AppState;

/// The single virtual model advertised to OpenAI-dialect clients. Routing is
/// decided by the panel, not the client's model picker.
pub const VIRTUAL_MODEL: &str = "big-brother";

fn openai_error(status: StatusCode, r#type: &str, message: &str) -> Response {
    (status, Json(openai_error_body(message, r#type, status.as_u16()))).into_response()
}

/// 404 for chat routes when config.toml has no [chat] section.
fn not_configured() -> Response {
    openai_error(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        "chat is not configured: add a [chat] section to config.toml",
    )
}

/// GET /v1/models -- exactly one virtual model.
pub async fn models(State(state): State<AppState>) -> Response {
    if state.chat.is_none() {
        return not_configured();
    }
    Json(json!({
        "object": "list",
        "data": [{
            "id": VIRTUAL_MODEL,
            "object": "model",
            "created": 0,
            "owned_by": "big-brother",
        }],
    }))
    .into_response()
}

/// Routing targets offered by the panel dropdown: "cascade" plus every
/// provider that has a configured default model.
fn targets(state: &AppState) -> Vec<String> {
    let mut list = vec!["cascade".to_string()];
    for (name, p) in &state.config.providers {
        if let Some(model) = &p.model {
            list.push(format!("{name}/{model}"));
        }
    }
    list
}

/// GET /chat/settings -- current settings plus the selectable targets.
pub async fn get_settings(State(state): State<AppState>) -> Response {
    let Some(chat) = &state.chat else { return not_configured() };
    let s = chat.get();
    Json(json!({
        "pipeline_enabled": s.pipeline_enabled,
        "model_override": s.model_override,
        "targets": targets(&state),
    }))
    .into_response()
}

/// PUT /chat/settings -- validate, apply, persist. Body: ChatSettings JSON.
pub async fn put_settings(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(chat) = &state.chat else { return not_configured() };
    let new: ChatSettings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid settings body: {e}"),
            )
        }
    };
    // "cascade" or "<known-provider>/<model>" only.
    if new.model_override != "cascade" {
        let valid = new
            .model_override
            .split_once('/')
            .is_some_and(|(p, m)| !m.is_empty() && state.config.providers.contains_key(p));
        if !valid {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("unknown routing target '{}'", new.model_override),
            );
        }
    }
    if let Err(e) = chat.set(new) {
        // In-memory state is updated; only persistence failed.
        tracing::error!(error = %e, "failed to persist chat settings");
        return openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            &format!("settings applied but not persisted: {e}"),
        );
    }
    get_settings(State(state)).await
}
```

- [ ] **Step 2: Wire it up** —
  - `src/lib.rs`: add `pub mod chat_proxy;`; in `build_state`, before `Ok(AppState { ... })`, add:

```rust
    let chat = config
        .chat
        .as_ref()
        .map(|c| Arc::new(crate::chat_settings::ChatState::load(c)));
```

  and add `chat,` to the `AppState` initializer.
  - `src/proxy.rs`: add `pub chat: Option<Arc<crate::chat_settings::ChatState>>,` to `AppState`; add routes in `router()`:

```rust
        .route("/v1/models", get(crate::chat_proxy::models))
        .route(
            "/chat/settings",
            get(crate::chat_proxy::get_settings).put(crate::chat_proxy::put_settings),
        )
```

  (`put` comes from `axum::routing::get(...).put(...)` — no extra import needed beyond the existing `get`.)

- [ ] **Step 3: Write integration tests** — create `tests/chat_integration.rs`:

```rust
//! Integration tests for the OpenAI-dialect chat routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use big_brother::config::Config;
use big_brother::{build_state, proxy};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Config with a [chat] section; state_file is unique per test to avoid races.
fn chat_config(upstream: &str, state_file: &str) -> String {
    std::env::set_var("CHAT_IT_KEY", "chat-secret");
    format!(
        r#"
        [default]
        provider = "primary"
        model = "primary-default-model"

        [chat]
        passthrough_url = "{upstream}/v1/chat/completions"
        passthrough_model = "local-model"
        state_file = "{state_file}"

        [providers.primary]
        base_url = "{upstream}/v1/messages"
        api_key_env = "CHAT_IT_KEY"
        model = "primary-default-model"
        "#
    )
}

fn temp_state(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("bb_chat_it_{name}_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&p);
    // TOML string: forward slashes work on Windows too and need no escaping.
    p.to_str().unwrap().replace('\\', "/")
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn put(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn models_lists_single_virtual_model() {
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &temp_state("models"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = get(app, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["id"], "big-brother");
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn chat_routes_404_openai_shaped_without_chat_config() {
    let toml = r#"
        [default]
        provider = "a"
        model = "m"

        [providers.a]
        base_url = "http://a.test/v1/messages"
        api_key_env = "A_KEY"
    "#;
    let app = proxy::router(build_state(Config::from_toml_str(toml).unwrap()).unwrap());
    let (status, body) = get(app, "/v1/models").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["message"].as_str().unwrap().contains("[chat]"));
}

#[tokio::test]
async fn settings_round_trip_and_validation() {
    let state_file = temp_state("settings");
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &state_file)).unwrap();
    let state = build_state(cfg).unwrap();

    let (status, body) = get(proxy::router(state.clone()), "/chat/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pipeline_enabled"], true);
    assert_eq!(body["model_override"], "cascade");
    assert_eq!(body["targets"], json!(["cascade", "primary/primary-default-model"]));

    // Valid update persists to the state file and is reflected in GET.
    let (status, body) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": false, "model_override": "primary/other-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pipeline_enabled"], false);
    assert_eq!(body["model_override"], "primary/other-model");
    let on_disk: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(on_disk["model_override"], "primary/other-model");

    // Unknown provider target is rejected.
    let (status, body) = put(
        proxy::router(state),
        "/chat/settings",
        json!({"pipeline_enabled": true, "model_override": "nope/model"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].as_str().unwrap().contains("nope/model"));
    let _ = std::fs::remove_file(&state_file);
}
```

- [ ] **Step 4: Run** — `cargo test --test chat_integration` and `cargo test` (full suite; existing tests must not break — `AppState` gained a field but is only constructed via `build_state`).
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: chat settings endpoints and /v1/models"`

---

### Task 7: `/v1/chat/completions` — pipeline path

**Files:**
- Modify: `src/chat_proxy.rs` (handler + response translation), `src/proxy.rs` (make `cascade` pub(crate); route), `src/metrics.rs` (chat counter)
- Test: `tests/chat_integration.rs`

**Interfaces:**
- Consumes: `proxy::cascade` (make `pub(crate)`), `proxy::forward` (already `pub(crate)`), `openai_compat::{openai_to_anthropic, anthropic_to_openai, SseTranslator, openai_error_body}`.
- Produces: route `POST /v1/chat/completions`; `metrics.chat_requests_total: IntCounterVec` labels `["mode"]`, name `bb_chat_requests_total`, modes `"pipeline"`/`"passthrough"`.

- [ ] **Step 1: Failing integration tests** — append to `tests/chat_integration.rs`:

```rust
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn post_chat(app: axum::Router, body: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn pipeline_chat_translates_both_directions() {
    let server = MockServer::start().await;
    // The upstream must receive an ANTHROPIC-dialect request with the default
    // model (no orchestrator configured -> forward to default provider).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "model": "primary-default-model",
            "system": "Be terse.",
            "messages": [{"role": "user", "content": "hi"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_7", "model": "primary-default-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hello back"}],
            "usage": {"input_tokens": 3, "output_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &temp_state("pipe"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(
        app,
        json!({
            "model": "big-brother",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "hi"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "hello back");
    assert_eq!(v["usage"]["total_tokens"], 5);
}

#[tokio::test]
async fn pipeline_chat_streams_openai_chunks() {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"model\":\"m\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &temp_state("sse"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(
        app,
        json!({"model": "big-brother", "stream": true,
               "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("chat.completion.chunk"), "body: {body}");
    assert!(body.contains("\"content\":\"Hi\""), "body: {body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "body: {body}");
}

#[tokio::test]
async fn explicit_model_override_routes_directly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "special-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_o", "model": "special-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let state_file = temp_state("override");
    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &state_file)).unwrap();
    let state = build_state(cfg).unwrap();
    let (status, _) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": true, "model_override": "primary/special-model"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_chat(
        proxy::router(state),
        json!({"model": "big-brother", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "ok");
    let _ = std::fs::remove_file(&state_file);
}

#[tokio::test]
async fn chat_escalates_through_cascade_on_sentinel() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_l", "model": "local-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "<<ESCALATE>>"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&local)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "cloud-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_c", "model": "cloud-model", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "cloud answer"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&cloud)
        .await;

    std::env::set_var("CHAT_IT_LOCAL_KEY", "k1");
    std::env::set_var("CHAT_IT_CLOUD_KEY", "k2");
    let toml = format!(
        r#"
        [default]
        provider = "local"
        model = "local-model"

        [chat]
        passthrough_url = "{lu}/v1/chat/completions"
        passthrough_model = "local-model"
        state_file = "{state}"

        [orchestrator]
        local_provider = "local"
        escalation_provider = "cloud"
        escalation_model = "cloud-model"

        [providers.local]
        base_url = "{lu}/v1/messages"
        api_key_env = "CHAT_IT_LOCAL_KEY"
        model = "local-model"

        [providers.cloud]
        base_url = "{cu}/v1/messages"
        api_key_env = "CHAT_IT_CLOUD_KEY"
        model = "cloud-model"
        "#,
        lu = local.uri(),
        cu = cloud.uri(),
        state = temp_state("cascade"),
    );
    let app = proxy::router(build_state(Config::from_toml_str(&toml).unwrap()).unwrap());
    let (status, body) = post_chat(
        app,
        json!({"model": "big-brother", "messages": [{"role": "user", "content": "hard question"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "cloud answer");
}

#[tokio::test]
async fn malformed_chat_request_is_400_openai_shaped() {
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &temp_state("bad"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let (status, body) = post_chat(app, json!({"model": "big-brother"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error");
}
```

- [ ] **Step 2: Verify failure** — `cargo test --test chat_integration` — expected: 404s (route not registered) / compile OK.

- [ ] **Step 3: Implement.**
  - `src/proxy.rs`: change `async fn cascade(` to `pub(crate) async fn cascade(` and register the route:

```rust
        .route("/v1/chat/completions", post(crate::chat_proxy::chat_completions))
```

  - `src/metrics.rs`: add field `pub chat_requests_total: IntCounterVec,` built as:

```rust
        let chat_requests_total = IntCounterVec::new(
            Opts::new(
                "bb_chat_requests_total",
                "OpenAI-dialect chat requests by mode (pipeline vs passthrough)",
            ),
            &["mode"],
        )
        .expect("valid metric");
```

  register it like the others and add to the struct initializer. Add label constants next to the tier ones:

```rust
pub const CHAT_MODE_PIPELINE: &str = "pipeline";
pub const CHAT_MODE_PASSTHROUGH: &str = "passthrough";
```

  - `src/chat_proxy.rs`: add the handler and response translation:

```rust
use std::sync::{Arc, Mutex};

use axum::body::Body;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;

use crate::error::AppError;
use crate::metrics;
use crate::openai_compat::{
    anthropic_to_openai, openai_to_anthropic, SseTranslator,
};
use crate::proxy;

/// POST /v1/chat/completions -- the OpenAI-dialect front door.
pub async fn chat_completions(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(chat) = state.chat.clone() else { return not_configured() };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {e}"),
            )
        }
    };
    let settings = chat.get();

    if !settings.pipeline_enabled {
        state
            .metrics
            .chat_requests_total
            .with_label_values(&[metrics::CHAT_MODE_PASSTHROUGH])
            .inc();
        return passthrough(&state, req).await;
    }

    state
        .metrics
        .chat_requests_total
        .with_label_values(&[metrics::CHAT_MODE_PIPELINE])
        .inc();
    let mut payload = match openai_to_anthropic(&req) {
        Ok(p) => p,
        Err(msg) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &msg)
        }
    };

    let result = if settings.model_override == "cascade" {
        payload["model"] = Value::String(state.config.default.model.clone());
        match state.orchestrator.clone() {
            Some(orch) => proxy::cascade(&state, &orch, payload).await,
            None => proxy::forward(&state, &state.config.default.provider, &payload).await,
        }
    } else {
        // Validated at PUT time; split cannot fail for stored settings, but
        // guard anyway (a hand-edited state file may hold anything).
        match settings.model_override.split_once('/') {
            Some((provider, model)) if state.config.providers.contains_key(provider) => {
                payload["model"] = Value::String(model.to_string());
                proxy::forward(&state, provider, &payload).await
            }
            _ => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("unknown routing target '{}'", settings.model_override),
                )
            }
        }
    };

    match result {
        Ok(resp) => translate_response(resp).await,
        Err(err) => app_error_to_openai(err),
    }
}

/// Convert the pipeline's AppError into an OpenAI-shaped error response.
fn app_error_to_openai(err: AppError) -> Response {
    let plain = err.into_response();
    let status = plain.status();
    let r#type = if status.is_server_error() { "api_error" } else { "invalid_request_error" };
    openai_error(status, r#type, &format!("upstream pipeline error ({status})"))
}

/// Translate a pipeline response (Anthropic dialect) to the OpenAI dialect.
/// SSE bodies stream through `SseTranslator`; JSON bodies are buffered and
/// converted; non-success statuses become OpenAI-shaped errors.
async fn translate_response(resp: Response) -> Response {
    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    if !status.is_success() {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap_or_default();
        let detail = String::from_utf8_lossy(&bytes);
        let r#type = if status.is_server_error() { "api_error" } else { "invalid_request_error" };
        return openai_error(status, r#type, &format!("upstream error: {detail}"));
    }

    if is_sse {
        let translator = Arc::new(Mutex::new(SseTranslator::new()));
        let map_t = translator.clone();
        let mapped = resp.into_body().into_data_stream().map(move |chunk| {
            chunk.map(|b| axum::body::Bytes::from(map_t.lock().unwrap().push(&b)))
        });
        let tail = futures_util::stream::once(async move {
            Ok(axum::body::Bytes::from(translator.lock().unwrap().finish()))
        });
        let mut out = Response::new(Body::from_stream(mapped.chain(tail)));
        *out.status_mut() = status;
        out.headers_mut()
            .insert(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        return out;
    }

    // Buffered JSON translation. 8 MB cap: chat completions are small.
    match axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => Json(anthropic_to_openai(&v)).into_response(),
            Err(e) => openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream returned unparseable JSON: {e}"),
            ),
        },
        Err(e) => openai_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("failed to read upstream body: {e}"),
        ),
    }
}
```

  (The `passthrough` function referenced above is Task 8; for THIS task's commit add a stub that returns `openai_error(StatusCode::NOT_IMPLEMENTED, "api_error", "passthrough not yet implemented")` so the crate compiles — Task 8 replaces it. The Task 7 tests only exercise the pipeline path.)

- [ ] **Step 4: Run** — `cargo test` (full suite) — expected: all pass, including the four new pipeline tests.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: /v1/chat/completions pipeline path with dialect translation"`

---

### Task 8: Passthrough mode

**Files:**
- Modify: `src/chat_proxy.rs` (replace stub)
- Test: `tests/chat_integration.rs`

**Interfaces:**
- Consumes: `ChatConfig.passthrough_url`, `ChatConfig.passthrough_model` (Task 1), `state.client`.

- [ ] **Step 1: Failing tests** — append to `tests/chat_integration.rs`:

```rust
#[tokio::test]
async fn passthrough_forwards_openai_dialect_with_model_rewrite() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "model": "local-model",
            "messages": [{"role": "user", "content": "hi"}],
            "some_openai_field": {"passed": "through"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-raw", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "raw"},
                         "finish_reason": "stop"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let state_file = temp_state("passthrough");
    let cfg = Config::from_toml_str(&chat_config(&server.uri(), &state_file)).unwrap();
    let state = build_state(cfg).unwrap();
    let (status, _) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": false, "model_override": "cascade"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_chat(
        proxy::router(state),
        json!({"model": "big-brother",
               "messages": [{"role": "user", "content": "hi"}],
               "some_openai_field": {"passed": "through"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Response comes back untouched -- no translation in passthrough mode.
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["id"], "chatcmpl-raw");
    let _ = std::fs::remove_file(&state_file);
}

#[tokio::test]
async fn passthrough_unreachable_upstream_is_502_openai_shaped() {
    // Port 1 is never listening.
    let state_file = temp_state("pt502");
    let cfg = Config::from_toml_str(&chat_config("http://127.0.0.1:1", &state_file)).unwrap();
    let state = build_state(cfg).unwrap();
    let (status, _) = put(
        proxy::router(state.clone()),
        "/chat/settings",
        json!({"pipeline_enabled": false, "model_override": "cascade"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_chat(
        proxy::router(state),
        json!({"model": "big-brother", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "api_error");
    let _ = std::fs::remove_file(&state_file);
}
```

- [ ] **Step 2: Verify failure** — `cargo test --test chat_integration passthrough` — expected: FAIL (stub returns 501).

- [ ] **Step 3: Implement** — replace the Task 7 stub in `src/chat_proxy.rs`:

```rust
/// Passthrough: forward the OpenAI request to the configured local endpoint,
/// rewriting only the model id. The response streams back verbatim.
async fn passthrough(state: &AppState, mut req: Value) -> Response {
    // The chat handler only calls this when state.chat is Some; config.chat
    // is Some whenever state.chat is (both derive from the same section).
    let Some(chat_cfg) = &state.config.chat else { return not_configured() };
    req["model"] = Value::String(chat_cfg.passthrough_model.clone());

    let upstream = match state
        .client
        .post(&chat_cfg.passthrough_url)
        .json(&req)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("failed to reach passthrough endpoint: {e}"),
            )
        }
    };
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let mut resp = Response::new(Body::from_stream(upstream.bytes_stream()));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, content_type.parse().unwrap());
    resp
}
```

- [ ] **Step 4: Run** — `cargo test` — expected: all pass.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: chat passthrough mode to local OpenAI endpoint"`

---

### Task 9: Metrics test + status snapshot

**Files:**
- Modify: `src/metrics.rs` (test), `src/proxy.rs` (`status` includes chat settings)
- Test: `tests/chat_integration.rs`

**Interfaces:**
- Produces: `GET /status` gains a `"chat"` key: the current `ChatSettings` (or `null` when unconfigured).

- [ ] **Step 1: Failing tests.** In `src/metrics.rs` `mod tests`:

```rust
#[test]
fn chat_counter_renders_by_mode() {
    let m = Metrics::new();
    m.chat_requests_total.with_label_values(&[CHAT_MODE_PIPELINE]).inc();
    let text = m.render();
    assert!(has_series(
        &text,
        "bb_chat_requests_total",
        &[("mode", "pipeline")],
        "1"
    ));
}
```

  In `tests/chat_integration.rs`:

```rust
#[tokio::test]
async fn status_includes_chat_settings() {
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &temp_state("status"))).unwrap();
    let (status, body) = get(proxy::router(build_state(cfg).unwrap()), "/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["chat"]["pipeline_enabled"], true);
    assert_eq!(body["chat"]["model_override"], "cascade");
}
```

- [ ] **Step 2: Verify failure** — `cargo test chat` — the status test fails (`body["chat"]` is null... actually missing).
- [ ] **Step 3: Implement** — in `src/proxy.rs::status`, add before the closing `Json(json!({...}))`:

```rust
    let chat = state.chat.as_ref().map(|c| c.get());
```

  and add `"chat": chat,` to the top-level `json!` object (`ChatSettings` derives `Serialize`).
- [ ] **Step 4: Run** — `cargo test` — expected: all pass.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: expose chat settings in /status and metrics"`

---

### Task 10: Panel Chat card

**Files:**
- Modify: `src/panel.html`
- Test: `tests/chat_integration.rs` (panel serves the card markup)

- [ ] **Step 1: Failing test** — append to `tests/chat_integration.rs`:

```rust
#[tokio::test]
async fn panel_contains_chat_card() {
    let cfg = Config::from_toml_str(&chat_config("http://unused.test", &temp_state("panel"))).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());
    let resp = app
        .oneshot(Request::builder().uri("/panel").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("id=\"chat-card\""));
    assert!(html.contains("id=\"chat-pipeline\""));
    assert!(html.contains("id=\"chat-target\""));
}
```

- [ ] **Step 2: Verify failure** — `cargo test --test chat_integration panel` — expected: FAIL.
- [ ] **Step 3: Implement.** In `src/panel.html`, insert this card directly after the Orchestrator card's closing `</div>` (line 58):

```html
  <div class="card" id="chat-card" hidden>
    <h2>Chat (Open WebUI)</h2>
    <div class="kv">
      <div>pipeline</div>
      <div><label><input type="checkbox" id="chat-pipeline"> route chat through the cascade/routing pipeline</label></div>
      <div>target</div>
      <div><select id="chat-target"></select></div>
    </div>
    <div id="chat-msg" class="empty"></div>
  </div>
```

  Add a matching style rule next to the existing ones (keeps the select legible on the dark theme):

```css
  select, input[type="checkbox"] { accent-color: var(--ok); }
  select { background:var(--bg); color:var(--text); border:1px solid var(--line);
           border-radius:4px; padding:4px 8px; font:inherit; }
```

  Add this script before the closing `</script>` (settings load once; edits PUT and re-render from the response; the card stays hidden when chat is unconfigured):

```javascript
async function loadChatSettings() {
  var card = document.getElementById("chat-card");
  try {
    var resp = await fetch("/chat/settings");
    if (!resp.ok) { card.hidden = true; return; }
    renderChatSettings(await resp.json());
    card.hidden = false;
  } catch (e) {
    card.hidden = true;
  }
}
function renderChatSettings(s) {
  document.getElementById("chat-pipeline").checked = s.pipeline_enabled;
  var sel = document.getElementById("chat-target");
  sel.innerHTML = s.targets.map(function (t) {
    return '<option value="' + esc(t) + '"' +
      (t === s.model_override ? " selected" : "") + ">" + esc(t) + "</option>";
  }).join("");
  sel.disabled = !s.pipeline_enabled;
}
async function saveChatSettings() {
  var msg = document.getElementById("chat-msg");
  var body = {
    pipeline_enabled: document.getElementById("chat-pipeline").checked,
    model_override: document.getElementById("chat-target").value
  };
  try {
    var resp = await fetch("/chat/settings", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body)
    });
    var data = await resp.json();
    if (!resp.ok) throw new Error(data.error && data.error.message || resp.status);
    renderChatSettings(data);
    msg.textContent = "saved";
    setTimeout(function () { msg.textContent = ""; }, 2000);
  } catch (e) {
    msg.textContent = "save failed: " + e.message;
    loadChatSettings();
  }
}
document.getElementById("chat-pipeline").addEventListener("change", saveChatSettings);
document.getElementById("chat-target").addEventListener("change", saveChatSettings);
loadChatSettings();
```

- [ ] **Step 4: Run** — `cargo test` — expected: all pass.
- [ ] **Step 5: Commit** — `git add src/panel.html tests/chat_integration.rs && git commit -m "feat: chat settings card on the status panel"`

---

### Task 11: Docker wiring + docs

**Files:**
- Modify: `docker-compose.yml`, `docker/config.toml`, `README.md`, `docs/ARCHITECTURE.md`, `docs/USER_GUIDE.md`

- [ ] **Step 1: docker-compose.yml.** In the `big-brother` service, add a data volume for the persisted settings:

```yaml
    volumes:
      - ./docker/config.toml:/app/config/config.toml:ro
      - big-brother-data:/app/data
```

  Replace the `open-webui` service's `environment` and `networks`:

```yaml
    environment:
      # Open WebUI now talks OpenAI dialect to Big Brother, which routes it
      # through the cascade pipeline (or passes through to LM Studio -- see
      # the Chat card on the panel at http://localhost:8787/panel).
      OPENAI_API_BASE_URL: "http://big-brother:8787/v1"
      OPENAI_API_KEY: "big-brother"
```

  and `networks: [webui, proxy]`. Register the new named volume at the bottom: `big-brother-data:`.

- [ ] **Step 2: docker/config.toml.** Append after the `[default]` section:

```toml
[chat]
# The Open WebUI front door. pipeline_enabled/model_override are DEFAULTS --
# the panel's Chat card edits them at runtime and persists to state_file.
passthrough_url = "http://192.168.1.10:8088/v1/chat/completions"
passthrough_model = "qwen3.6:27b"
state_file = "/app/data/chat_state.json"
```

- [ ] **Step 3: Validate** — `docker compose config` — expected: renders without error, open-webui shows both networks and the big-brother URL.
- [ ] **Step 4: Docs.** Update:
  - `README.md`: add the two new endpoints to any endpoint list; one paragraph: Open WebUI chats through Big Brother, controlled from the panel's Chat card.
  - `docs/ARCHITECTURE.md`: add the chat flow (Open WebUI → `/v1/chat/completions` → translate → pipeline → translate back; passthrough bypass) to the endpoint/diagram sections.
  - `docs/USER_GUIDE.md`: a "Chat window" section: how to flip the pipeline toggle and pick a target, that settings persist, and that Open WebUI's model picker shows only `big-brother` by design.
- [ ] **Step 5: Full verification** — `cargo test` (everything green) and `cargo clippy --all-targets` (no new warnings).
- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat: route Open WebUI through the proxy; document the chat front door"`
