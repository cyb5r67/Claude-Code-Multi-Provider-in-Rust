# Status Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read-only observability for Big Brother — a `GET /status` JSON endpoint and an embedded auto-polling `GET /panel` HTML page showing orchestrator state, budget usage, sticky conversations, and the last 500 escalations.

**Architecture:** The orchestrator gains an in-memory escalation history (ring buffer + totals) and a serializable `status()` snapshot; the proxy records escalation events at the two decision points in `escalate()` and exposes two new GET routes on the existing listener. The panel is one self-contained HTML file compiled in via `include_str!`.

**Tech Stack:** Rust 2021, axum 0.7, serde/serde_json. No new dependencies. Vanilla HTML/CSS/JS for the panel.

**Spec:** `docs/superpowers/specs/2026-08-13-status-panel-design.md`

## Global Constraints

- No new dependencies; no auth (localhost-only listener, read-only routes).
- Timestamps are Unix epoch seconds (`at_epoch_secs: u64`); the page renders local time in JS.
- `ESCALATION_HISTORY_CAP = 500`; history and counters are in-memory and reset on restart.
- Triggers recorded: `sentinel`, `sticky`, `fail_mode` (count in `total_since_start`) and `budget_denied` (counts in `budget_denied_since_start` only).
- Privacy: no message content; conversation keys appear only as first-8-char prefixes; API keys only as `api_key_present` booleans.
- `/status` and `/panel` must never call upstream providers.
- `orchestrator` field in `/status` is JSON `null` when the section is absent or disabled.
- `cargo fmt` before every commit; `cargo test` green at the end of every task. Existing tests stay untouched.

---

### Task 1: Orchestrator history, budget/sticky readers, and `status()` snapshot

**Files:**
- Modify: `src/config.rs` (add `Serialize` to two derives)
- Modify: `src/orchestrator.rs`

**Interfaces:**
- Consumes: existing `Orchestrator` (fields `cfg`, `sticky: Mutex<HashMap<String, Tier>>`, `cloud_calls: Mutex<VecDeque<Instant>>`), `config::{OrchestratorConfig, FailMode}`.
- Produces: `orchestrator::ESCALATION_HISTORY_CAP: usize = 500`; `orchestrator::EscalationRecord { at_epoch_secs: u64, trigger: String, provider: String, model: String, conversation_key_prefix: Option<String> }` (Serialize); `Orchestrator::record_escalation(&self, trigger: &str, provider: &str, model: &str, key: Option<&str>)`; `Orchestrator::budget_used(&self) -> u32` and `budget_used_at(&self, now: Instant) -> u32` (non-consuming); `Orchestrator::sticky_count(&self) -> usize`; `Orchestrator::status(&self) -> OrchestratorStatus` where `OrchestratorStatus { enabled: bool, local_provider: String, escalation_provider: String, escalation_model: String, sentinel: String, fail_mode: FailMode, budget: BudgetStatus, sticky_cloud_conversations: usize, escalations: EscalationsStatus }`, `BudgetStatus { max_per_hour: u32, used_last_hour: u32, remaining: u32 }`, `EscalationsStatus { total_since_start: u64, budget_denied_since_start: u64, recent: Vec<EscalationRecord> }` — all `Serialize`, `recent` newest-first. `config::AuthStyle` and `config::FailMode` become `Serialize` (lowercase).

- [ ] **Step 1: Add `Serialize` in `src/config.rs`** — change the import `use serde::Deserialize;` to `use serde::{Deserialize, Serialize};`, and add `Serialize` to the derive lists of `AuthStyle` and `FailMode` (both keep `#[serde(rename_all = "lowercase")]`, which applies to serialization too):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthStyle {
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
```

- [ ] **Step 2: Write the failing tests** — append inside the existing `tests` module of `src/orchestrator.rs` (its `orch(max_per_hour)` helper and `Duration`/`Instant` imports already exist):

```rust
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
        assert_eq!(v["escalations"]["recent"][0]["conversation_key_prefix"], "aabbccdd");
        let at = v["escalations"]["recent"][0]["at_epoch_secs"].as_u64().unwrap();
        assert!(at > 1_700_000_000, "expected a current epoch timestamp, got {at}");
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib orchestrator`
Expected: compile errors — `record_escalation`, `status`, `budget_used_at`, `ESCALATION_HISTORY_CAP` not defined.

- [ ] **Step 4: Implement** — in `src/orchestrator.rs`. Extend imports: add `FailMode` to the existing `crate::config` import, and add:

```rust
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
```

Add the types and constant (near `Tier`):

```rust
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
```

Add the field `history: Mutex<History>` to `Orchestrator` and initialize it in `new()` with `Mutex::new(History::default())`.

Refactor the prune loop out of `try_reserve_cloud_call_at` into a shared helper, and add the new methods:

```rust
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
```

`try_reserve_cloud_call_at` becomes:

```rust
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
```

New methods on `impl Orchestrator`:

```rust
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
            sticky_cloud_conversations: self.sticky_count(),
            escalations: EscalationsStatus {
                total_since_start: history.total_escalations,
                budget_denied_since_start: history.total_budget_denied,
                recent: history.records.iter().rev().cloned().collect(),
            },
        }
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib orchestrator` then `cargo test`
Expected: 17 orchestrator tests pass (12 existing + 5 new); full suite green.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/config.rs src/orchestrator.rs
git commit -m "Add escalation history and status snapshot to the orchestrator"
```

---

### Task 2: `GET /status` endpoint and recording call sites

**Files:**
- Modify: `src/proxy.rs` (`router`, `escalate`, new handler)
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: Task 1's `record_escalation`, `status()`; existing `AppState`, `escalate()` (which has a budget-denied branch and a granted branch), `Provider::api_key()`.
- Produces: `GET /status` returning the spec's JSON shape (`proxy` object + `orchestrator` object-or-null). Task 3 relies only on this route existing.

- [ ] **Step 1: Write the failing integration tests** — append to `tests/proxy_integration.rs` (helpers `config_toml`, `orchestrated_config_toml`, `send`, `sentinel_json_response` already exist there):

```rust
/// GET a route on the router and parse the JSON body.
async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn status_reports_null_orchestrator_when_disabled() {
    let server = MockServer::start().await;
    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let (status, body) = get_json(app, "/status").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["orchestrator"], Value::Null);
    assert_eq!(body["proxy"]["default_provider"], "primary");
    let providers = body["proxy"]["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    // BTreeMap order: primary then secondary; env vars are set by config_toml.
    assert_eq!(providers[0]["name"], "primary");
    assert_eq!(providers[0]["api_key_present"], true);
    assert_eq!(providers[0]["auth_style"], "bearer");
    // No key material anywhere in the response.
    assert!(!body.to_string().contains("primary-secret"));
}

#[tokio::test]
async fn status_reflects_escalation_budget_and_sticky() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
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
    let state = build_state(cfg).unwrap();

    let (s1, _) = send(
        proxy::router(state.clone()),
        json!({"model": "m", "messages": [{"role": "user", "content": "hard question"}]}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, body) = get_json(proxy::router(state), "/status").await;
    assert_eq!(s2, StatusCode::OK);
    let orch = &body["orchestrator"];
    assert_eq!(orch["enabled"], true);
    assert_eq!(orch["fail_mode"], "cloud");
    assert_eq!(orch["budget"]["used_last_hour"], 1);
    assert_eq!(orch["budget"]["remaining"], 9);
    assert_eq!(orch["sticky_cloud_conversations"], 1);
    assert_eq!(orch["escalations"]["total_since_start"], 1);
    let recent = orch["escalations"]["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["trigger"], "sentinel");
    assert_eq!(recent[0]["provider"], "cloud");
    assert_eq!(recent[0]["model"], "cloud-big-model");
    assert_eq!(
        recent[0]["conversation_key_prefix"].as_str().unwrap().len(),
        8
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test proxy_integration status_`
Expected: both FAIL — `/status` does not exist yet (404, body `Null`).

- [ ] **Step 3: Implement** — in `src/proxy.rs`.

Add the handler (near `health`):

```rust
/// Read-only status snapshot for the panel. Never calls upstream providers.
async fn status(State(state): State<AppState>) -> Json<Value> {
    let providers: Vec<Value> = state
        .config
        .providers
        .iter()
        .map(|(name, p)| {
            json!({
                "name": name,
                "base_url": p.base_url,
                "auth_style": p.auth_style,
                "api_key_present": p.api_key().is_some(),
            })
        })
        .collect();
    let orchestrator = state.orchestrator.as_ref().map(|o| o.status());
    Json(json!({
        "proxy": {
            "version": env!("CARGO_PKG_VERSION"),
            "default_provider": state.config.default.provider,
            "default_model": state.config.default.model,
            "providers": providers,
        },
        "orchestrator": orchestrator,
    }))
}
```

Register it in `router()`:

```rust
        .route("/status", get(status))
```

Add the two recording call sites in `escalate()`:

In the budget-denied branch, immediately after the `tracing::warn!` and before building the fallback payload:

```rust
        let local_model = state
            .config
            .providers
            .get(&orch.cfg.local_provider)
            .and_then(|p| p.model.clone())
            .unwrap_or_default();
        orch.record_escalation("budget_denied", &orch.cfg.local_provider, &local_model, key);
```

In the granted path, immediately after the existing `tracing::info!(... "escalating to cloud tier")`:

```rust
    orch.record_escalation(
        trigger,
        &orch.cfg.escalation_provider,
        &orch.cfg.escalation_model,
        key,
    );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: full suite green including the two new integration tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/proxy.rs tests/proxy_integration.rs
git commit -m "Expose GET /status and record escalation events"
```

---

### Task 3: Embedded panel page and `GET /panel` route

**Files:**
- Create: `src/panel.html`
- Modify: `src/proxy.rs` (one handler + route)
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: `GET /status` (Task 2) — the page's only data source.
- Produces: `GET /panel` serving the embedded page as `text/html`.

- [ ] **Step 1: Write the failing integration test** — append to `tests/proxy_integration.rs`:

```rust
#[tokio::test]
async fn panel_serves_embedded_html() {
    let server = MockServer::start().await;
    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let req = Request::builder()
        .method("GET")
        .uri("/panel")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/html"));
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&bytes);
    assert!(html.contains("Big Brother status"));
    assert!(html.contains("fetchStatus"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test proxy_integration panel_`
Expected: FAIL — 404, no `/panel` route.

- [ ] **Step 3: Create `src/panel.html`** with exactly this content:

```html
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Big Brother status</title>
<style>
  :root { --bg:#12141a; --card:#1c2028; --text:#e6e8ee; --muted:#8a92a6;
          --line:#2a3040; --ok:#4ade80; --warn:#fbbf24; --bad:#f87171; }
  * { box-sizing: border-box; }
  body { margin:0; padding:24px; background:var(--bg); color:var(--text);
         font:14px/1.5 ui-monospace, "Cascadia Mono", Consolas, monospace; }
  .wrap { max-width:900px; margin:0 auto; }
  header { display:flex; align-items:center; gap:12px; margin-bottom:20px; }
  h1 { font-size:18px; margin:0; }
  .ver { color:var(--muted); }
  .dot { width:10px; height:10px; border-radius:50%; background:var(--ok); }
  .dot.down { background:var(--bad); }
  .stale { opacity:.45; }
  .card { background:var(--card); border:1px solid var(--line); border-radius:8px;
          padding:16px; margin-bottom:16px; }
  .card h2 { font-size:13px; margin:0 0 10px; color:var(--muted);
             text-transform:uppercase; letter-spacing:.08em; }
  .kv { display:grid; grid-template-columns:auto 1fr; gap:2px 16px; }
  .kv div:nth-child(odd) { color:var(--muted); }
  .tiles { display:flex; gap:16px; }
  .tile { flex:1; text-align:center; }
  .tile .n { font-size:26px; }
  .tile .l { color:var(--muted); font-size:12px; }
  .bar { height:10px; background:var(--line); border-radius:5px; overflow:hidden;
         margin:8px 0 4px; }
  .bar span { display:block; height:100%; background:var(--ok); }
  .bar.warn span { background:var(--warn); }
  .bar.bad span { background:var(--bad); }
  table { width:100%; border-collapse:collapse; }
  th, td { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); }
  th { color:var(--muted); font-weight:normal; font-size:12px; }
  .chip { padding:1px 8px; border-radius:10px; font-size:12px; }
  .chip.sentinel { background:#3b2f5b; color:#c4b5fd; }
  .chip.sticky { background:#173a4d; color:#7dd3fc; }
  .chip.fail_mode { background:#4d3a17; color:#fcd34d; }
  .chip.budget_denied { background:#4d1f1f; color:#fca5a5; }
  .empty { color:var(--muted); }
  .yes { color:var(--ok); } .no { color:var(--bad); }
</style>
</head>
<body>
<div class="wrap" id="wrap">
  <header>
    <span class="dot" id="dot"></span>
    <h1>Big Brother status</h1>
    <span class="ver" id="version"></span>
    <span class="ver" id="state">connecting&hellip;</span>
  </header>
  <div class="card">
    <h2>Orchestrator</h2>
    <div id="orch-body" class="kv"></div>
  </div>
  <div class="card" id="budget-card" hidden>
    <h2>Cloud budget (last hour)</h2>
    <div class="bar" id="bar"><span id="bar-fill" style="width:0%"></span></div>
    <div id="budget-label" class="empty"></div>
  </div>
  <div class="card" id="tiles-card" hidden>
    <div class="tiles">
      <div class="tile"><div class="n" id="t-sticky">0</div><div class="l">sticky cloud conversations</div></div>
      <div class="tile"><div class="n" id="t-esc">0</div><div class="l">escalations since start</div></div>
      <div class="tile"><div class="n" id="t-denied">0</div><div class="l">budget-denied since start</div></div>
    </div>
  </div>
  <div class="card" id="esc-card" hidden>
    <h2>Recent escalations</h2>
    <div id="esc-body"></div>
  </div>
  <div class="card">
    <h2>Providers</h2>
    <div id="prov-body"></div>
  </div>
</div>
<script>
function esc(s) {
  return String(s).replace(/[&<>"']/g, function (c) {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
  });
}
function render(data) {
  document.getElementById("version").textContent = "v" + data.proxy.version;
  var o = data.orchestrator;
  ["budget-card", "tiles-card", "esc-card"].forEach(function (id) {
    document.getElementById(id).hidden = !o;
  });
  var ob = document.getElementById("orch-body");
  if (!o) {
    ob.innerHTML = '<div>state</div><div class="empty">orchestrator disabled</div>';
  } else {
    ob.innerHTML =
      "<div>tiers</div><div>" + esc(o.local_provider) + " → " + esc(o.escalation_provider) + "</div>" +
      "<div>escalation model</div><div>" + esc(o.escalation_model) + "</div>" +
      "<div>sentinel</div><div>" + esc(o.sentinel) + "</div>" +
      "<div>fail mode</div><div>" + esc(o.fail_mode) + "</div>";
    var b = o.budget;
    var pct = b.max_per_hour ? Math.round(100 * b.used_last_hour / b.max_per_hour) : 0;
    var bar = document.getElementById("bar");
    bar.className = "bar" + (pct >= 100 ? " bad" : pct >= 80 ? " warn" : "");
    document.getElementById("bar-fill").style.width = Math.min(pct, 100) + "%";
    document.getElementById("budget-label").textContent =
      b.used_last_hour + " / " + b.max_per_hour + " used, " + b.remaining + " remaining";
    document.getElementById("t-sticky").textContent = o.sticky_cloud_conversations;
    document.getElementById("t-esc").textContent = o.escalations.total_since_start;
    document.getElementById("t-denied").textContent = o.escalations.budget_denied_since_start;
    var rows = o.escalations.recent.map(function (r) {
      return "<tr><td>" + new Date(r.at_epoch_secs * 1000).toLocaleTimeString() + "</td>" +
        '<td><span class="chip ' + esc(r.trigger) + '">' + esc(r.trigger) + "</span></td>" +
        "<td>" + esc(r.provider) + " / " + esc(r.model) + "</td>" +
        "<td>" + esc(r.conversation_key_prefix || "—") + "</td></tr>";
    }).join("");
    document.getElementById("esc-body").innerHTML = rows
      ? "<table><tr><th>time</th><th>trigger</th><th>target</th><th>conversation</th></tr>" + rows + "</table>"
      : '<div class="empty">No escalations yet.</div>';
  }
  document.getElementById("prov-body").innerHTML =
    "<table><tr><th>name</th><th>base_url</th><th>auth</th><th>key</th></tr>" +
    data.proxy.providers.map(function (p) {
      return "<tr><td>" + esc(p.name) + "</td><td>" + esc(p.base_url) + "</td>" +
        "<td>" + esc(p.auth_style) + "</td>" +
        (p.api_key_present ? '<td class="yes">present</td>' : '<td class="no">missing</td>') + "</tr>";
    }).join("") +
    "</table>";
}
async function fetchStatus() {
  var dot = document.getElementById("dot");
  var state = document.getElementById("state");
  var wrap = document.getElementById("wrap");
  try {
    var resp = await fetch("/status");
    if (!resp.ok) throw new Error(resp.status);
    render(await resp.json());
    dot.classList.remove("down");
    state.textContent = "live";
    wrap.classList.remove("stale");
  } catch (e) {
    dot.classList.add("down");
    state.textContent = "proxy unreachable";
    wrap.classList.add("stale");
  }
}
fetchStatus();
setInterval(fetchStatus, 3000);
</script>
</body>
</html>
```

- [ ] **Step 4: Add the handler and route** — in `src/proxy.rs`, add `Html` to the axum response imports (`use axum::response::{Html, IntoResponse, Response};`), then:

```rust
/// The embedded status panel (single self-contained file, no external assets).
async fn panel() -> Html<&'static str> {
    Html(include_str!("panel.html"))
}
```

and in `router()`:

```rust
        .route("/panel", get(panel))
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: full suite green including `panel_serves_embedded_html`.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/panel.html src/proxy.rs tests/proxy_integration.rs
git commit -m "Serve the embedded status panel at GET /panel"
```

---

### Task 4: Documentation and final verification

**Files:**
- Modify: `README.md`
- Modify: `docs/USER_GUIDE.md`

- [ ] **Step 1: README** — in the "How it works" bullet list, add after the hierarchical-orchestrator bullet:

```markdown
- `GET /panel` — a read-only status page (orchestrator state, budget, recent
  escalations); `GET /status` serves the same data as JSON.
```

In the Layout table, add rows for the files Phase 1 and this feature introduced (after the `src/proxy.rs` row):

```markdown
| `src/orchestrator.rs` | Escalation state: sticky map, budget, history      |
| `src/stream.rs`       | Sentinel detection over SSE/JSON responses          |
| `src/panel.html`      | Embedded status panel served at `/panel`            |
```

- [ ] **Step 2: USER_GUIDE** — insert a new section between the "Hierarchical orchestrator" section (after its Limitations subsection and closing `---`) and "Example: local LM Studio hosts":

```markdown
## Status panel

Open <http://127.0.0.1:8787/panel> in a browser while the proxy is running.
The page polls `GET /status` every 3 seconds and shows:

- orchestrator state (tiers, escalation model, sentinel, fail mode);
- cloud budget for the sliding hour, with a bar that turns amber at 80%;
- sticky cloud conversations and escalation totals;
- the last 500 escalations (time, trigger, target, conversation-key prefix);
- configured providers and whether each API-key env var is set.

Both routes are read-only, never call upstream providers, and are only
reachable from the machine running the proxy (the default `127.0.0.1` bind).
No message content or key material appears: conversations show only as
8-character hash prefixes, keys only as present/missing. History and
counters reset when the proxy restarts.

`GET /status` returns the same data as JSON if you'd rather script against
it (`curl http://127.0.0.1:8787/status`).

---
```

Also add `- [Status panel](#status-panel)` to the table-of-contents list at the top of the guide, after the "Switching providers and models" entry.

- [ ] **Step 3: Final verification**

Run: `cargo fmt --check && cargo test`
Expected: no formatting drift; every test passes.

Manual smoke check (optional, no live providers needed): `cargo run` with the checked-in config, open `http://127.0.0.1:8787/panel` — the page renders with "orchestrator disabled" and the providers table.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/USER_GUIDE.md
git commit -m "Document the status panel"
```
