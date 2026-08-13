# Hierarchical Orchestrator Phase 1 (Sentinel Cascade) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Big Brother forwards every fresh conversation to a local Qwen tier first; when Qwen opens its reply with a sentinel token, the proxy transparently replays the request to a cloud tier (direct Anthropic support), with sticky per-conversation escalation, an hourly budget cap, and an audit log.

**Architecture:** A `cascade` branch in the proxy handler runs when the orchestrator is enabled and the request did not explicitly select a provider. It injects a sentinel instruction into a clone of the payload, inspects the local response's leading text (JSON or SSE), and either passes the response through or replays the untouched original payload to the escalation provider. State (sticky map, budget window) lives in a new `orchestrator` module; response inspection lives in a new `stream` module.

**Tech Stack:** Rust 2021, axum 0.7, reqwest 0.12 (rustls), serde_json, tokio, futures-util, wiremock (tests). One new dependency: `sha2 = "0.10"`.

**Spec:** `docs/superpowers/specs/2026-08-13-hierarchical-orchestrator-design.md`

## Global Constraints

- Crate name is `big_brother` (package `big-brother`). Do not rename anything.
- Only new dependency allowed: `sha2 = "0.10"`.
- Default sentinel literal: `<<ESCALATE>>`.
- Anthropic auth style sends headers `x-api-key: <key>` and `anthropic-version: 2023-06-01`, and does NOT send `authorization`.
- The existing static routing behavior (`/model` commands, model-field routing) must not change; all pre-existing tests must keep passing untouched except for the `resolve_route` signature change in Task 3.
- Sticky state and budget counters are in-memory only (reset on restart).
- Run `cargo fmt` before every commit. `cargo test` must pass at the end of every task.
- Windows dev box: commands are plain `cargo ...`, no shell tricks needed.

---

### Task 1: Config — `AuthStyle` on `Provider` and the `[orchestrator]` section

**Files:**
- Modify: `src/config.rs`
- Modify: `Cargo.toml` (no change needed here; `sha2` is added in Task 6)

**Interfaces:**
- Produces: `config::AuthStyle` (`Bearer` | `Anthropic`, default `Bearer`), field `Provider.auth_style: AuthStyle`; `config::FailMode` (`Cloud` | `Error`, default `Cloud`); `config::OrchestratorConfig { enabled: bool (default true), local_provider: String, escalation_provider: String, escalation_model: String, sentinel: String (default "<<ESCALATE>>"), max_cloud_requests_per_hour: u32 (default 50), fail_mode: FailMode }`; field `Config.orchestrator: Option<OrchestratorConfig>`.

- [ ] **Step 1: Write the failing tests** — append to the `tests` module in `src/config.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config`
Expected: compile errors — `AuthStyle`, `FailMode`, `orchestrator` not defined.

- [ ] **Step 3: Implement** — in `src/config.rs`, add below the `Provider` struct:

```rust
/// How to authenticate against a provider's endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthStyle {
    /// `authorization: Bearer <key>` plus `x-api-key` (y-router compatibility).
    #[default]
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version: 2023-06-01` (api.anthropic.com).
    Anthropic,
}

/// Behavior when the local tier fails before a sentinel verdict is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
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

fn default_true() -> bool {
    true
}
fn default_sentinel() -> String {
    "<<ESCALATE>>".to_string()
}
fn default_max_cloud_requests_per_hour() -> u32 {
    50
}
```

Add to `Provider`:

```rust
    /// How to authenticate; defaults to the Bearer style used by y-router-like
    /// endpoints.
    #[serde(default)]
    pub auth_style: AuthStyle,
```

Add to `Config`:

```rust
    #[serde(default)]
    pub orchestrator: Option<OrchestratorConfig>,
```

The `provider(...)` helper in the existing tests constructs `Provider` literally; add `auth_style: AuthStyle::default(),` to that constructor.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all tests pass (existing + 4 new).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/config.rs
git commit -m "Add auth_style to providers and the [orchestrator] config section"
```

---

### Task 2: Auth-style-aware upstream headers

**Files:**
- Modify: `src/proxy.rs`
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: `config::AuthStyle` (Task 1).
- Produces: `proxy::apply_auth(req: reqwest::RequestBuilder, style: AuthStyle, api_key: &str) -> reqwest::RequestBuilder` (crate-visible, `pub(crate)`).

- [ ] **Step 1: Write the failing integration test** — append to `tests/proxy_integration.rs`:

```rust
/// Providers with auth_style = "anthropic" get x-api-key + anthropic-version
/// headers (api.anthropic.com rejects requests without the version header).
#[tokio::test]
async fn anthropic_auth_style_sends_version_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "anthropic-secret"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "anthropic"})))
        .expect(1)
        .mount(&server)
        .await;

    std::env::set_var("IT_ANTHROPIC_KEY", "anthropic-secret");
    let toml = format!(
        r#"
        [default]
        provider = "anthropic"
        model = "claude-opus-5"

        [providers.anthropic]
        base_url = "{}/v1/messages"
        api_key_env = "IT_ANTHROPIC_KEY"
        auth_style = "anthropic"
        "#,
        server.uri()
    );
    let cfg = Config::from_toml_str(&toml).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "anthropic"}));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test proxy_integration anthropic_auth_style`
Expected: FAIL — the mock never matches because no `anthropic-version` header is sent (wiremock returns 404, assertion on status fails).

- [ ] **Step 3: Implement** — in `src/proxy.rs`, add the helper and use it in `messages_proxy`:

```rust
use crate::config::AuthStyle;

/// Attach the provider's authentication headers to an outgoing request.
pub(crate) fn apply_auth(
    req: reqwest::RequestBuilder,
    style: AuthStyle,
    api_key: &str,
) -> reqwest::RequestBuilder {
    match style {
        AuthStyle::Bearer => req
            .header("authorization", format!("Bearer {api_key}"))
            .header("x-api-key", api_key),
        AuthStyle::Anthropic => req
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
    }
}
```

Replace the two `.header(...)` lines in `messages_proxy`'s upstream build with:

```rust
    let upstream = apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(&payload)
    .send()
    .await
    .map_err(|source| AppError::Upstream {
        provider: provider_key.clone(),
        source,
    })?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all pass, including the pre-existing header assertions (Bearer style unchanged).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/proxy.rs tests/proxy_integration.rs
git commit -m "Send anthropic-version header for auth_style=anthropic providers"
```

---

### Task 3: `resolve_route` reports whether routing was explicit

**Files:**
- Modify: `src/proxy.rs` (function + its unit tests)

**Interfaces:**
- Produces: `proxy::RouteSource` (`Default` | `Explicit`, `Copy + PartialEq + Debug`); `resolve_route` now returns `(String, String, RouteSource)`. `Explicit` means a `/model` text command, a `provider/model` model field, or a bare provider name selected the provider; `Default` means defaults applied or the model passed through unrecognized.

- [ ] **Step 1: Change the signature and tag each branch**

```rust
/// Whether the request explicitly selected a provider (human override) or fell
/// through to defaults. The orchestrator only engages for `Default` routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteSource {
    Default,
    Explicit,
}

fn resolve_route(cfg: &Config, payload: &mut Value) -> (String, String, RouteSource) {
    let mut provider_key = cfg.default.provider.clone();
    let mut model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.default.model)
        .to_string();
    let mut source = RouteSource::Default;

    if let Some(cmd) = model_command::parse_and_strip(payload) {
        tracing::info!(provider = %cmd.provider, model = %cmd.model, "model switch via /model command");
        provider_key = cmd.provider;
        model = cmd.model;
        source = RouteSource::Explicit;
    } else if let Some((prefix, rest)) = model.split_once('/') {
        if !rest.is_empty() && cfg.providers.contains_key(prefix) {
            tracing::info!(provider = %prefix, model = %rest, "model switch via model field");
            provider_key = prefix.to_string();
            model = rest.to_string();
            source = RouteSource::Explicit;
        }
    } else if let Some(default_model) = cfg.providers.get(&model).and_then(|p| p.model.clone()) {
        tracing::info!(provider = %model, model = %default_model, "provider switch via model field");
        provider_key = std::mem::replace(&mut model, default_model);
        source = RouteSource::Explicit;
    }

    payload["model"] = Value::String(model.clone());
    (provider_key, model, source)
}
```

Update the call site in `messages_proxy`:

```rust
    let (provider_key, model, _source) = resolve_route(cfg, &mut payload);
```

- [ ] **Step 2: Update the unit tests** — every existing `resolve_route` test in `src/proxy.rs` destructures a 2-tuple; change each to `let (provider, model, _source) = ...`. Then strengthen two of them and add coverage:

In `defaults_apply_when_body_has_no_model`:

```rust
    let (provider, model, source) = resolve_route(&cfg(), &mut payload);
    assert_eq!(source, RouteSource::Default);
```

In `provider_prefixed_model_field_switches_provider`:

```rust
    let (provider, model, source) = resolve_route(&cfg(), &mut payload);
    assert_eq!(source, RouteSource::Explicit);
```

New tests:

```rust
#[test]
fn passthrough_model_is_default_source() {
    let mut payload = json!({"model": "x-ai/grok-code-fast-1", "messages": []});
    let (_, _, source) = resolve_route(&cfg(), &mut payload);
    assert_eq!(source, RouteSource::Default);
}

#[test]
fn text_command_is_explicit_source() {
    let mut payload = json!({
        "messages": [{"role": "user", "content": "/model beta/some-model hi"}]
    });
    let (_, _, source) = resolve_route(&cfg(), &mut payload);
    assert_eq!(source, RouteSource::Explicit);
}

#[test]
fn bare_provider_name_is_explicit_source() {
    let mut payload = json!({"model": "beta", "messages": []});
    let (_, _, source) = resolve_route(&cfg(), &mut payload);
    assert_eq!(source, RouteSource::Explicit);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add src/proxy.rs
git commit -m "resolve_route reports whether the route was explicitly selected"
```

---

### Task 4: `stream` module — `check_sentinel`

**Files:**
- Create: `src/stream.rs`
- Modify: `src/lib.rs` (add `pub mod stream;`)

**Interfaces:**
- Produces: `stream::SentinelVerdict` (`Sentinel` | `Clean` | `Undetermined`, `Copy + PartialEq + Debug`); `stream::check_sentinel(accumulated: &str, sentinel: &str) -> SentinelVerdict`. Semantics: leading whitespace ignored; `Sentinel` if the trimmed text starts with the sentinel; `Undetermined` if the trimmed text is still a proper prefix of the sentinel (including empty); `Clean` otherwise.

- [ ] **Step 1: Write the failing tests** — create `src/stream.rs`:

```rust
//! Sentinel detection over model responses (Phase 1 cascade).

/// Result of inspecting the leading text of a local-tier response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentinelVerdict {
    /// The response begins with the sentinel: escalate.
    Sentinel,
    /// The response cannot begin with the sentinel: pass through.
    Clean,
    /// Not enough text yet to decide.
    Undetermined,
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "<<ESCALATE>>";

    #[test]
    fn empty_text_is_undetermined() {
        assert_eq!(check_sentinel("", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn partial_prefix_is_undetermined() {
        assert_eq!(check_sentinel("<<ESC", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn exact_sentinel_is_detected() {
        assert_eq!(check_sentinel("<<ESCALATE>>", S), SentinelVerdict::Sentinel);
    }

    #[test]
    fn sentinel_with_trailing_text_is_detected() {
        assert_eq!(
            check_sentinel("<<ESCALATE>> this needs the big model", S),
            SentinelVerdict::Sentinel
        );
    }

    #[test]
    fn leading_whitespace_is_ignored() {
        assert_eq!(check_sentinel("\n <<ESCALATE>>", S), SentinelVerdict::Sentinel);
        assert_eq!(check_sentinel("\n ", S), SentinelVerdict::Undetermined);
    }

    #[test]
    fn ordinary_text_is_clean() {
        assert_eq!(check_sentinel("The answer is 4.", S), SentinelVerdict::Clean);
    }

    #[test]
    fn sentinel_mid_text_is_clean() {
        // Only the FIRST token counts (prompt-injection defense).
        assert_eq!(
            check_sentinel("As the file says: <<ESCALATE>>", S),
            SentinelVerdict::Clean
        );
    }
}
```

Add `pub mod stream;` to `src/lib.rs` (alphabetical order with the other modules).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib stream`
Expected: compile error — `check_sentinel` not defined.

- [ ] **Step 3: Implement** — add above the tests module:

```rust
/// Decide whether `accumulated` (the response text so far) begins with the
/// sentinel. Leading whitespace is ignored; anything after the first token is
/// ordinary content (a sentinel appearing mid-text never escalates).
pub fn check_sentinel(accumulated: &str, sentinel: &str) -> SentinelVerdict {
    let text = accumulated.trim_start();
    if text.starts_with(sentinel) {
        SentinelVerdict::Sentinel
    } else if sentinel.starts_with(text) {
        SentinelVerdict::Undetermined
    } else {
        SentinelVerdict::Clean
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib stream`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/stream.rs src/lib.rs
git commit -m "Add sentinel verdict check for cascade response inspection"
```

---

### Task 5: `stream` module — SSE scanner and JSON text extraction

**Files:**
- Modify: `src/stream.rs`

**Interfaces:**
- Consumes: `check_sentinel`, `SentinelVerdict` (Task 4).
- Produces: `stream::SseTextScanner` with `new(sentinel: String) -> Self`, `push(&mut self, chunk: &bytes::Bytes) -> SentinelVerdict`, `into_buffered(self) -> Vec<bytes::Bytes>`; `stream::json_first_text(body: &serde_json::Value) -> Option<&str>`. The scanner buffers every raw chunk verbatim (for later release to the client), extracts assistant text from `content_block_start`/`content_block_delta` SSE events, and returns a verdict after each push. `bytes` is already a transitive dependency via axum/reqwest; add `bytes = "1"` to `[dependencies]` so it can be named directly.

- [ ] **Step 1: Add the `bytes` dependency** — in `Cargo.toml` under `[dependencies]`:

```toml
bytes = "1"
```

- [ ] **Step 2: Write the failing tests** — append to the `tests` module in `src/stream.rs`:

```rust
    use bytes::Bytes;
    use serde_json::json;

    fn sse_line(event: &serde_json::Value) -> String {
        format!("data: {event}\n\n")
    }

    #[test]
    fn scanner_detects_sentinel_in_first_delta() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let start = sse_line(&json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}
        }));
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESCALATE>>"}
        }));
        assert_eq!(
            scanner.push(&Bytes::from(start)),
            SentinelVerdict::Undetermined
        );
        assert_eq!(scanner.push(&Bytes::from(delta)), SentinelVerdict::Sentinel);
    }

    #[test]
    fn scanner_handles_sentinel_split_across_deltas_and_chunks() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let d1 = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "<<ESC"}
        }));
        let d2 = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "ALATE>>"}
        }));
        // Split the second event's bytes mid-line to exercise line buffering.
        let d2_bytes = d2.into_bytes();
        let (head, tail) = d2_bytes.split_at(10);

        assert_eq!(scanner.push(&Bytes::from(d1)), SentinelVerdict::Undetermined);
        assert_eq!(
            scanner.push(&Bytes::copy_from_slice(head)),
            SentinelVerdict::Undetermined
        );
        assert_eq!(
            scanner.push(&Bytes::copy_from_slice(tail)),
            SentinelVerdict::Sentinel
        );
    }

    #[test]
    fn scanner_rules_out_sentinel_on_ordinary_text() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let delta = sse_line(&json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        }));
        assert_eq!(scanner.push(&Bytes::from(delta)), SentinelVerdict::Clean);
    }

    #[test]
    fn scanner_ignores_non_data_lines_and_bad_json() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let noise = "event: message_start\ndata: {not json}\n\n";
        assert_eq!(
            scanner.push(&Bytes::from(noise)),
            SentinelVerdict::Undetermined
        );
    }

    #[test]
    fn scanner_returns_all_raw_chunks_verbatim() {
        let mut scanner = SseTextScanner::new(S.to_string());
        let c1 = Bytes::from("event: message_start\n");
        let c2 = Bytes::from("data: {\"type\":\"message_start\"}\n\n");
        scanner.push(&c1);
        scanner.push(&c2);
        assert_eq!(scanner.into_buffered(), vec![c1, c2]);
    }

    #[test]
    fn json_first_text_reads_first_text_block() {
        let body = json!({
            "content": [
                {"type": "text", "text": "<<ESCALATE>>"},
                {"type": "text", "text": "ignored"}
            ]
        });
        assert_eq!(json_first_text(&body), Some("<<ESCALATE>>"));
        assert_eq!(json_first_text(&json!({"content": []})), None);
        assert_eq!(json_first_text(&json!({"no": "content"})), None);
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib stream`
Expected: compile errors — `SseTextScanner`, `json_first_text` not defined.

- [ ] **Step 4: Implement** — add to `src/stream.rs`:

```rust
use bytes::Bytes;
use serde_json::Value;

/// Incrementally scans an Anthropic-format SSE byte stream for the sentinel,
/// buffering every raw chunk so a clean stream can be released to the client
/// unmodified.
///
/// Note: chunks are decoded lossily per-chunk; a multi-byte character split
/// across a chunk boundary may be mangled in the *scanned text* only. The
/// sentinel is ASCII and appears first, so detection is unaffected, and the
/// client always receives the untouched raw bytes.
pub struct SseTextScanner {
    sentinel: String,
    raw: Vec<Bytes>,
    pending: String,
    text: String,
}

impl SseTextScanner {
    pub fn new(sentinel: String) -> Self {
        SseTextScanner {
            sentinel,
            raw: Vec::new(),
            pending: String::new(),
            text: String::new(),
        }
    }

    /// Feed one raw chunk; returns the verdict so far.
    pub fn push(&mut self, chunk: &Bytes) -> SentinelVerdict {
        self.raw.push(chunk.clone());
        self.pending.push_str(&String::from_utf8_lossy(chunk));
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<Value>(data) {
                    self.absorb_event(&event);
                }
            }
        }
        check_sentinel(&self.text, &self.sentinel)
    }

    fn absorb_event(&mut self, event: &Value) {
        let text = match event.get("type").and_then(Value::as_str) {
            Some("content_block_start") => event.pointer("/content_block/text"),
            Some("content_block_delta") => event.pointer("/delta/text"),
            _ => None,
        };
        if let Some(t) = text.and_then(Value::as_str) {
            self.text.push_str(t);
        }
    }

    /// All raw chunks fed so far, verbatim, for release to the client.
    pub fn into_buffered(self) -> Vec<Bytes> {
        self.raw
    }
}

/// First text block's content from a non-streaming Messages response body.
pub fn json_first_text(body: &Value) -> Option<&str> {
    body.get("content")?
        .as_array()?
        .iter()
        .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))?
        .get("text")?
        .as_str()
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib stream`
Expected: 13 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/stream.rs
git commit -m "Add SSE text scanner and JSON first-text extraction"
```

---

### Task 6: `orchestrator` module — conversation key

**Files:**
- Create: `src/orchestrator.rs`
- Modify: `src/lib.rs` (add `pub mod orchestrator;`)
- Modify: `Cargo.toml` (add `sha2 = "0.10"`)

**Interfaces:**
- Produces: `orchestrator::conversation_key(payload: &serde_json::Value) -> Option<String>` — SHA-256 hex of the first user message's text content. Plain-string content hashes the string; content-block arrays hash the `text` fields of `type == "text"` blocks joined with `\n`; no user message or non-text-only content returns `None` (callers then skip stickiness, never fail the request).

- [ ] **Step 1: Add the dependency** — in `Cargo.toml` under `[dependencies]`:

```toml
sha2 = "0.10"
```

- [ ] **Step 2: Write the failing tests** — create `src/orchestrator.rs`:

```rust
//! Escalation state and payload mutation for the hierarchical orchestrator.

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
```

Add `pub mod orchestrator;` to `src/lib.rs`.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib orchestrator`
Expected: compile error — `conversation_key` not defined.

- [ ] **Step 4: Implement** — add above the tests module:

```rust
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
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib orchestrator`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/orchestrator.rs src/lib.rs
git commit -m "Add sticky-conversation key derivation"
```

---

### Task 7: `orchestrator` module — sticky map and budget window

**Files:**
- Modify: `src/orchestrator.rs`

**Interfaces:**
- Consumes: `config::OrchestratorConfig` (Task 1).
- Produces: `orchestrator::Tier` (`Local` | `Cloud`, `Copy + PartialEq + Debug`); `orchestrator::Orchestrator` with `new(cfg: OrchestratorConfig) -> Self`, public field `cfg: OrchestratorConfig`, `sticky_tier(&self, key: &str) -> Option<Tier>`, `mark_cloud(&self, key: &str)`, `try_reserve_cloud_call(&self) -> bool`, and test seam `try_reserve_cloud_call_at(&self, now: std::time::Instant) -> bool` (sliding one-hour window; reserving pushes the timestamp).

- [ ] **Step 1: Write the failing tests** — append inside the `tests` module:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib orchestrator`
Expected: compile errors — `Orchestrator`, `Tier` not defined.

- [ ] **Step 3: Implement** — add to `src/orchestrator.rs`:

```rust
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
        self.sticky.lock().unwrap().insert(key.to_string(), Tier::Cloud);
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib orchestrator`
Expected: 8 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/orchestrator.rs
git commit -m "Add orchestrator sticky map and sliding-window cloud budget"
```

---

### Task 8: `orchestrator` module — system-note injection

**Files:**
- Modify: `src/orchestrator.rs`

**Interfaces:**
- Produces: `orchestrator::sentinel_instruction(sentinel: &str) -> String`; `orchestrator::append_system_note(payload: &mut serde_json::Value, note: &str)` (handles `system` as string, block array, or absent); `orchestrator::ESCALATION_UNAVAILABLE_NOTE: &str`. The proxy never strips the injected note — it keeps a pre-injection clone of the payload instead (established in Task 10).

- [ ] **Step 1: Write the failing tests** — append inside the `tests` module:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib orchestrator`
Expected: compile errors — functions not defined.

- [ ] **Step 3: Implement** — add to `src/orchestrator.rs`:

```rust
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib orchestrator`
Expected: 12 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/orchestrator.rs
git commit -m "Add sentinel instruction and system-note injection helpers"
```

---

### Task 9: Wire the orchestrator into `AppState`; extract the `forward` helper

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/proxy.rs`
- Modify: `src/main.rs`

This is a behavior-preserving refactor plus state plumbing; existing tests are the safety net.

**Interfaces:**
- Consumes: `Orchestrator` (Task 7), `apply_auth` (Task 2).
- Produces: `proxy::AppState` gains `pub orchestrator: Option<std::sync::Arc<crate::orchestrator::Orchestrator>>` (built by `build_state` when the config section is present and enabled); `proxy::forward(state: &AppState, provider_key: &str, payload: &serde_json::Value) -> Result<axum::response::Response, AppError>` (async, `pub(crate)`) — provider lookup, auth, send, error-body buffering, stream passthrough: exactly the behavior `messages_proxy` has today after route resolution.

- [ ] **Step 1: Extract `forward` in `src/proxy.rs`** — move everything in `messages_proxy` after route resolution into:

```rust
/// Forward a resolved payload to the named provider, streaming the response
/// through (buffering only error bodies for logging).
pub(crate) async fn forward(
    state: &AppState,
    provider_key: &str,
    payload: &Value,
) -> Result<Response, AppError> {
    let provider = state
        .config
        .providers
        .get(provider_key)
        .ok_or_else(|| AppError::UnknownProvider(provider_key.to_string()))?;
    let api_key = provider.api_key().ok_or_else(|| AppError::MissingApiKey {
        provider: provider_key.to_string(),
        env: provider.api_key_env.clone(),
    })?;

    tracing::info!(provider = %provider_key, base_url = %provider.base_url, "forwarding request");

    let upstream = apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(payload)
    .send()
    .await
    .map_err(|source| AppError::Upstream {
        provider: provider_key.to_string(),
        source,
    })?;

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| "application/json".parse().unwrap());

    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        tracing::warn!(provider = %provider_key, %status, body = %preview, "upstream returned error status");
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(response.into_response());
    }

    let stream = upstream.bytes_stream();
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    Ok(response.into_response())
}
```

`messages_proxy` becomes:

```rust
async fn messages_proxy(State(state): State<AppState>, body: Bytes) -> Result<Response, AppError> {
    let cfg = &state.config;

    let mut payload: Value =
        serde_json::from_slice(&body).map_err(|e| AppError::InvalidJson(e.to_string()))?;

    let (provider_key, model, source) = resolve_route(cfg, &mut payload);
    tracing::info!(provider = %provider_key, %model, "routing request");

    // The cascade branch lands in Task 10; `source` is used there.
    let _ = source;

    forward(&state, &provider_key, &payload).await
}
```

- [ ] **Step 2: Add the orchestrator to `AppState`** — in `src/proxy.rs`:

```rust
use crate::orchestrator::Orchestrator;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    pub orchestrator: Option<Arc<Orchestrator>>,
}
```

In `src/lib.rs`, `build_state` becomes:

```rust
use orchestrator::Orchestrator;

pub fn build_state(config: Config) -> Result<AppState, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.server.request_timeout_secs))
        .build()?;
    let orchestrator = config
        .orchestrator
        .as_ref()
        .filter(|o| o.enabled)
        .map(|o| Arc::new(Orchestrator::new(o.clone())));
    Ok(AppState {
        config: Arc::new(config),
        client,
        orchestrator,
    })
}

/// Log the orchestrator's startup posture so a misconfigured tier is visible
/// immediately (unknown providers still fail per-request with 400s).
pub fn log_orchestrator(config: &Config) {
    match &config.orchestrator {
        Some(o) if o.enabled => {
            for key in [&o.local_provider, &o.escalation_provider] {
                if !config.providers.contains_key(key) {
                    tracing::warn!(provider = %key, "orchestrator references unknown provider");
                }
            }
            tracing::info!(
                local = %o.local_provider,
                cloud = %o.escalation_provider,
                model = %o.escalation_model,
                budget_per_hour = o.max_cloud_requests_per_hour,
                "orchestrator enabled"
            );
        }
        Some(_) => tracing::info!("orchestrator section present but disabled"),
        None => {}
    }
}
```

In `src/main.rs`, after the `log_key_presence(&config);` line add:

```rust
    big_brother::log_orchestrator(&config);
```

- [ ] **Step 3: Run all tests**

Run: `cargo test`
Expected: everything passes unchanged (pure refactor + additive state).

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add src/lib.rs src/proxy.rs src/main.rs
git commit -m "Extract forward() and wire orchestrator state into AppState"
```

---

### Task 10: Non-streaming cascade (escalate, pass-through, sticky, budget)

**Files:**
- Modify: `src/proxy.rs`
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: everything above. `RouteSource::Default` gates the cascade; explicit routes and disabled orchestrator use `forward` as before.
- Produces: `proxy::cascade` (private async fn), `proxy::escalate` (private), `proxy::local_attempt` (private) returning the private enum `LocalOutcome { Clean(Response), Escalate, Failed(Response) }`. Local attempts override `model` with the local provider's configured default `model` when set; escalations override `model` with `orchestrator.cfg.escalation_model`. The SSE branch of `local_attempt` is a stub in this task (treats `text/event-stream` like passthrough `Clean`); Task 11 replaces it.

- [ ] **Step 1: Write the failing integration tests** — append to `tests/proxy_integration.rs`:

```rust
use big_brother::orchestrator::{sentinel_instruction, ESCALATION_UNAVAILABLE_NOTE};
use wiremock::matchers::body_partial_json;

/// Config with the orchestrator enabled: "local" is the Qwen stand-in,
/// "cloud" the Anthropic stand-in.
fn orchestrated_config_toml(
    local_url: &str,
    cloud_url: &str,
    max_per_hour: u32,
    fail_mode: &str,
) -> String {
    std::env::set_var("IT_LOCAL_KEY", "local-secret");
    std::env::set_var("IT_CLOUD_KEY", "cloud-secret");
    format!(
        r#"
        [default]
        provider = "local"
        model = "local-default-model"

        [orchestrator]
        local_provider = "local"
        escalation_provider = "cloud"
        escalation_model = "cloud-big-model"
        max_cloud_requests_per_hour = {max_per_hour}
        fail_mode = "{fail_mode}"

        [providers.local]
        base_url = "{local_url}/v1/messages"
        api_key_env = "IT_LOCAL_KEY"
        model = "local-model"

        [providers.cloud]
        base_url = "{cloud_url}/v1/messages"
        api_key_env = "IT_CLOUD_KEY"
        auth_style = "anthropic"
        "#
    )
}

fn sentinel_json_response() -> Value {
    json!({"content": [{"type": "text", "text": "<<ESCALATE>>"}]})
}

#[tokio::test]
async fn sentinel_response_escalates_to_cloud() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Local attempt: model overridden to the local default, sentinel
    // instruction injected as the system prompt.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({
            "model": "local-model",
            "system": sentinel_instruction("<<ESCALATE>>")
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(1)
        .mount(&local)
        .await;

    // Escalation: the ORIGINAL payload (no injected system prompt) with the
    // model swapped to the escalation model, Anthropic-style headers.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-api-key", "cloud-secret"))
        .and(body_json(json!({
            "model": "cloud-big-model",
            "messages": [{"role": "user", "content": "hard question"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "whatever-model",
            "messages": [{"role": "user", "content": "hard question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn clean_response_is_answered_locally() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"content": [{"type": "text", "text": "The answer is 4."}]}),
        ))
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "whatever-model",
            "messages": [{"role": "user", "content": "what is 2+2"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"][0]["text"], "The answer is 4.");
}

#[tokio::test]
async fn escalated_conversation_is_sticky() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Local is attempted exactly once (turn 1); turn 2 skips it.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(2)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let state = build_state(cfg).unwrap();

    let turn1 = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hard question"}]
    });
    let turn2 = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": "hard question"},
            {"role": "assistant", "content": "cloud answer"},
            {"role": "user", "content": "follow-up"}
        ]
    });

    let (s1, _) = send(proxy::router(state.clone()), turn1).await;
    let (s2, b2) = send(proxy::router(state), turn2).await;

    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn exhausted_budget_falls_back_to_local_with_note() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    // Budget-denied fallback: original payload + escalation-unavailable note.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"system": ESCALATION_UNAVAILABLE_NOTE})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"content": [{"type": "text", "text": "best local effort"}]}),
        ))
        .expect(1)
        .mount(&local)
        .await;

    // Sentinel attempts (conversations A and B).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(2)
        .mount(&local)
        .await;

    // Budget of 1: only conversation A gets through.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 1, "cloud"))
            .unwrap();
    let state = build_state(cfg).unwrap();

    let conv_a = json!({"model": "m", "messages": [{"role": "user", "content": "conversation A"}]});
    let conv_b = json!({"model": "m", "messages": [{"role": "user", "content": "conversation B"}]});

    let (sa, ba) = send(proxy::router(state.clone()), conv_a).await;
    let (sb, bb) = send(proxy::router(state), conv_b).await;

    assert_eq!(sa, StatusCode::OK);
    assert_eq!(ba, json!({"routed": "cloud"}));
    assert_eq!(sb, StatusCode::OK);
    assert_eq!(bb["content"][0]["text"], "best local effort");
}

/// Explicit provider selection bypasses the cascade entirely.
#[tokio::test]
async fn explicit_model_selection_bypasses_orchestrator() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_json(json!({
            "model": "picked-model",
            "messages": [{"role": "user", "content": "hi"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud-explicit"})))
        .expect(1)
        .mount(&cloud)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&local)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    // `cloud/picked-model` explicitly names the cloud provider.
    let (status, body) = send(
        app,
        json!({
            "model": "cloud/picked-model",
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud-explicit"}));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test proxy_integration`
Expected: the five new tests FAIL (requests go straight to the default provider; cascade doesn't exist yet). All pre-existing tests still pass.

- [ ] **Step 3: Implement the cascade** — in `src/proxy.rs`. New imports:

```rust
use std::sync::Arc;

use crate::config::FailMode;
use crate::orchestrator::{self, Orchestrator, Tier};
use crate::stream::{self, SentinelVerdict};
```

Gate in `messages_proxy` (replace the `let _ = source;` placeholder):

```rust
    if source == RouteSource::Default {
        if let Some(orch) = state.orchestrator.clone() {
            return cascade(&state, &orch, payload).await;
        }
    }
```

The cascade and its helpers:

```rust
/// Outcome of the local-tier attempt.
enum LocalOutcome {
    /// Deliver the local response as-is.
    Clean(Response),
    /// Sentinel detected: run the escalation path.
    Escalate,
    /// Local tier answered with an HTTP error (buffered pass-through body).
    Failed(Response),
}

/// Phase 1 sentinel cascade: try the local tier, escalate on sentinel.
async fn cascade(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    original: Value,
) -> Result<Response, AppError> {
    let key = orchestrator::conversation_key(&original);

    if let Some(k) = key.as_deref() {
        if orch.sticky_tier(k) == Some(Tier::Cloud) {
            return escalate(state, orch, key.as_deref(), &original, "sticky").await;
        }
    }

    let mut attempt = original.clone();
    set_local_model(state, orch, &mut attempt);
    orchestrator::append_system_note(
        &mut attempt,
        &orchestrator::sentinel_instruction(&orch.cfg.sentinel),
    );

    match local_attempt(state, orch, &attempt).await {
        Ok(LocalOutcome::Clean(response)) => Ok(response),
        Ok(LocalOutcome::Escalate) => {
            escalate(state, orch, key.as_deref(), &original, "sentinel").await
        }
        Ok(LocalOutcome::Failed(response)) => match orch.cfg.fail_mode {
            FailMode::Cloud => {
                tracing::warn!("local tier returned an error; escalating per fail_mode=cloud");
                escalate(state, orch, key.as_deref(), &original, "fail_mode").await
            }
            FailMode::Error => Ok(response),
        },
        Err(err) => match orch.cfg.fail_mode {
            FailMode::Cloud => {
                tracing::warn!(error = %err, "local tier unreachable; escalating per fail_mode=cloud");
                escalate(state, orch, key.as_deref(), &original, "fail_mode").await
            }
            FailMode::Error => Err(err),
        },
    }
}

/// Local attempts run against the local provider's own default model when one
/// is configured (the client's model id is meaningless to LM Studio).
fn set_local_model(state: &AppState, orch: &Arc<Orchestrator>, payload: &mut Value) {
    if let Some(local_model) = state
        .config
        .providers
        .get(&orch.cfg.local_provider)
        .and_then(|p| p.model.clone())
    {
        payload["model"] = Value::String(local_model);
    }
}

/// Route to the cloud tier, honoring the budget. On a denied budget the
/// request is answered locally with an "escalation unavailable" note.
async fn escalate(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    key: Option<&str>,
    original: &Value,
    trigger: &str,
) -> Result<Response, AppError> {
    if !orch.try_reserve_cloud_call() {
        tracing::warn!(
            trigger,
            budget_per_hour = orch.cfg.max_cloud_requests_per_hour,
            "cloud budget exhausted; answering locally"
        );
        let mut fallback = original.clone();
        set_local_model(state, orch, &mut fallback);
        orchestrator::append_system_note(&mut fallback, orchestrator::ESCALATION_UNAVAILABLE_NOTE);
        return forward(state, &orch.cfg.local_provider, &fallback).await;
    }

    if let Some(k) = key {
        orch.mark_cloud(k);
    }

    let mut cloud = original.clone();
    cloud["model"] = Value::String(orch.cfg.escalation_model.clone());
    // The audit line: one per escalation, greppable.
    tracing::info!(
        trigger,
        provider = %orch.cfg.escalation_provider,
        model = %orch.cfg.escalation_model,
        "escalating to cloud tier"
    );
    forward(state, &orch.cfg.escalation_provider, &cloud).await
}

/// Send the sentinel-instrumented attempt to the local tier and inspect the
/// leading response text. SSE inspection lands in Task 11; until then
/// streaming responses pass through as Clean.
async fn local_attempt(
    state: &AppState,
    orch: &Arc<Orchestrator>,
    attempt: &Value,
) -> Result<LocalOutcome, AppError> {
    let provider_key = &orch.cfg.local_provider;
    let provider = state
        .config
        .providers
        .get(provider_key)
        .ok_or_else(|| AppError::UnknownProvider(provider_key.clone()))?;
    let api_key = provider.api_key().ok_or_else(|| AppError::MissingApiKey {
        provider: provider_key.clone(),
        env: provider.api_key_env.clone(),
    })?;

    let upstream = apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(attempt)
    .send()
    .await
    .map_err(|source| AppError::Upstream {
        provider: provider_key.clone(),
        source,
    })?;

    let status = upstream.status();
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| "application/json".parse().unwrap());

    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]);
        tracing::warn!(provider = %provider_key, %status, body = %preview, "local tier returned error status");
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(LocalOutcome::Failed(response.into_response()));
    }

    let is_sse = content_type
        .to_str()
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        // Task 11 replaces this stub with sentinel scanning.
        let body_stream = upstream.bytes_stream();
        let mut response = Response::new(Body::from_stream(body_stream));
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, content_type);
        return Ok(LocalOutcome::Clean(response.into_response()));
    }

    let bytes = upstream
        .bytes()
        .await
        .map_err(|source| AppError::Upstream {
            provider: provider_key.clone(),
            source,
        })?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = stream::json_first_text(&body).unwrap_or("");
    if stream::check_sentinel(text, &orch.cfg.sentinel) == SentinelVerdict::Sentinel {
        return Ok(LocalOutcome::Escalate);
    }

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    Ok(LocalOutcome::Clean(response.into_response()))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all pass, including the five new integration tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/proxy.rs tests/proxy_integration.rs
git commit -m "Add sentinel cascade for non-streaming responses"
```

---

### Task 11: Streaming (SSE) cascade

**Files:**
- Modify: `src/proxy.rs` (replace the SSE stub in `local_attempt`)
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: `SseTextScanner` (Task 5).
- Produces: the SSE branch of `local_attempt` scans chunks until a verdict; on `Clean` (or end-of-stream while `Undetermined`) it releases the buffered chunks chained with the remaining upstream stream; on `Sentinel` it returns `LocalOutcome::Escalate`.

- [ ] **Step 1: Write the failing integration tests** — append to `tests/proxy_integration.rs`:

```rust
fn sse_body(first_text: &str, rest: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\"}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{first_text}\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{rest}\"}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// Send and return the raw body string (for SSE responses).
async fn send_raw(app: axum::Router, body: Value) -> (StatusCode, String, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string(), content_type)
}

#[tokio::test]
async fn sse_sentinel_escalates_to_cloud() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_body("<<ESCALATE>>", ""), "text/event-stream"),
        )
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "cloud-big-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "hard streaming question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn sse_clean_response_streams_through_verbatim() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    let full_body = sse_body("Hello", " there");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(full_body.clone(), "text/event-stream"),
        )
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body, content_type) = send_raw(
        app,
        json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "easy streaming question"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("text/event-stream"));
    // Every byte the local tier produced reaches the client unmodified.
    assert_eq!(body, full_body);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test proxy_integration sse_`
Expected: `sse_sentinel_escalates_to_cloud` FAILS (the stub passes the sentinel SSE through instead of escalating). `sse_clean_response_streams_through_verbatim` may already pass via the stub — that's fine; it pins the passthrough contract before the rewrite.

- [ ] **Step 3: Implement** — replace the SSE stub in `local_attempt` with:

```rust
    if is_sse {
        let mut scanner = stream::SseTextScanner::new(orch.cfg.sentinel.clone());
        let mut byte_stream = upstream.bytes_stream();
        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => match scanner.push(&chunk) {
                    SentinelVerdict::Sentinel => return Ok(LocalOutcome::Escalate),
                    SentinelVerdict::Clean => {
                        let head = futures_util::stream::iter(
                            scanner.into_buffered().into_iter().map(Ok::<_, reqwest::Error>),
                        );
                        let body = Body::from_stream(head.chain(byte_stream));
                        let mut response = Response::new(body);
                        *response.status_mut() = status;
                        response.headers_mut().insert(CONTENT_TYPE, content_type);
                        return Ok(LocalOutcome::Clean(response.into_response()));
                    }
                    SentinelVerdict::Undetermined => continue,
                },
                Some(Err(source)) => {
                    return Err(AppError::Upstream {
                        provider: provider_key.clone(),
                        source,
                    })
                }
                None => {
                    // Stream ended before a verdict (short response that is a
                    // proper prefix of the sentinel): deliver what we have.
                    let head = futures_util::stream::iter(
                        scanner.into_buffered().into_iter().map(Ok::<_, reqwest::Error>),
                    );
                    let mut response = Response::new(Body::from_stream(head));
                    *response.status_mut() = status;
                    response.headers_mut().insert(CONTENT_TYPE, content_type);
                    return Ok(LocalOutcome::Clean(response.into_response()));
                }
            }
        }
    }
```

Add the import at the top of `src/proxy.rs`:

```rust
use futures_util::StreamExt;
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/proxy.rs tests/proxy_integration.rs
git commit -m "Scan SSE streams for the sentinel and release clean streams verbatim"
```

---

### Task 12: `fail_mode` integration coverage

**Files:**
- Test: `tests/proxy_integration.rs`

The `fail_mode` logic already exists (Task 10); this task pins it with tests. If a test exposes a defect, fix it in `src/proxy.rs` within this task.

- [ ] **Step 1: Write the tests**

```rust
#[tokio::test]
async fn local_http_error_escalates_when_fail_mode_cloud() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "local down"})))
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({"model": "m", "messages": [{"role": "user", "content": "q"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}

#[tokio::test]
async fn local_http_error_passes_through_when_fail_mode_error() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "local down"})))
        .expect(1)
        .mount(&local)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "error"))
            .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({"model": "m", "messages": [{"role": "user", "content": "q"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error": "local down"}));
}

#[tokio::test]
async fn unreachable_local_escalates_when_fail_mode_cloud() {
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    // 127.0.0.1:1 refuses connections instantly.
    let cfg = Config::from_toml_str(&orchestrated_config_toml(
        "http://127.0.0.1:1",
        &cloud.uri(),
        10,
        "cloud",
    ))
    .unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = send(
        app,
        json!({"model": "m", "messages": [{"role": "user", "content": "q"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"routed": "cloud"}));
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --test proxy_integration fail_mode -- --nocapture` then `cargo test`
Expected: all pass (logic landed in Task 10). If any fail, fix `cascade`'s `Failed`/`Err` arms accordingly.

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add tests/proxy_integration.rs
git commit -m "Pin fail_mode behavior with integration tests"
```

---

### Task 13: Documentation and final verification

**Files:**
- Modify: `config.toml`
- Modify: `README.md`
- Modify: `docs/USER_GUIDE.md`

- [ ] **Step 1: Add a commented-out orchestrator example to `config.toml`** — append:

```toml
# --- Hierarchical orchestrator (optional) -----------------------------------
# When enabled, fresh conversations are answered by the local tier first; if
# the local model opens its reply with the sentinel token, the request is
# transparently replayed to the escalation provider. An explicit
# "/model provider/model" always bypasses orchestration.
#
# [orchestrator]
# local_provider = "qwen"                 # must name a [providers.*] entry
# escalation_provider = "anthropic"       # must name a [providers.*] entry
# escalation_model = "claude-opus-5"
# # sentinel = "<<ESCALATE>>"             # default
# # max_cloud_requests_per_hour = 50      # default; budget guard
# # fail_mode = "cloud"                   # "cloud" (default) or "error"
#
# [providers.anthropic]
# base_url = "https://api.anthropic.com/v1/messages"
# api_key_env = "ANTHROPIC_API_KEY"
# auth_style = "anthropic"                # sends x-api-key + anthropic-version
```

- [ ] **Step 2: Document the feature in `docs/USER_GUIDE.md`** — add a section after "Switching providers and models":

```markdown
---

## Hierarchical orchestrator (local-first with cloud escalation)

With an `[orchestrator]` section in the config, Big Brother answers every
fresh conversation with the **local tier** first (e.g. Qwen on LM Studio). The
proxy injects a system instruction telling the local model to output the
sentinel token (`<<ESCALATE>>` by default) as its very first token when a task
is beyond it. When that happens, the proxy silently replays the original
request to the **escalation provider** and the conversation stays on the cloud
tier until it ends ("sticky").

```toml
[orchestrator]
local_provider = "qwen"
escalation_provider = "anthropic"
escalation_model = "claude-opus-5"

[providers.qwen]
base_url = "http://192.168.1.10:8088/anthropic/v1/messages"
api_key_env = "LMSTUDIO"
model = "qwen3.6:27b"

[providers.anthropic]
base_url = "https://api.anthropic.com/v1/messages"
api_key_env = "ANTHROPIC_API_KEY"
auth_style = "anthropic"
```

Rules of thumb:

- **You always win:** `/model provider/model` bypasses orchestration for that
  request.
- **Budget guard:** at most `max_cloud_requests_per_hour` escalations per hour
  (default 50). Beyond that, requests are answered locally and a warning is
  logged.
- **Audit:** every escalation logs one line —
  `escalating to cloud tier trigger=sentinel provider=anthropic model=claude-opus-5`.
- **Local tier down?** `fail_mode = "cloud"` (default) escalates;
  `fail_mode = "error"` surfaces the error as before.
- Local attempts are sent with the local provider's configured `model`;
  escalations are sent with `escalation_model`.
```

- [ ] **Step 3: Mention it in `README.md`** — in the "How it works" bullet list, after the `/health` bullet, add:

```markdown
- Optional **hierarchical orchestrator**: answer conversations with a local
  model first and transparently escalate to a cloud model when the local tier
  signals a task is beyond it. See the
  [user guide](docs/USER_GUIDE.md#hierarchical-orchestrator-local-first-with-cloud-escalation).
```

- [ ] **Step 4: Full verification**

Run: `cargo fmt --check && cargo test`
Expected: no formatting drift; every test passes.

Manual acceptance (requires live LM Studio + an Anthropic key; record results in the commit message or a follow-up note):
1. Start Big Brother with orchestration enabled, point Claude Code at it.
2. Trivial prompt → answered by Qwen; log shows no `escalating to cloud tier` line.
3. Hard reasoning prompt → log shows one escalation; answer arrives normally.
4. `/model anthropic/claude-opus-5` → bypasses orchestration (log shows `model switch via model field`).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add config.toml README.md docs/USER_GUIDE.md
git commit -m "Document the hierarchical orchestrator"
```
