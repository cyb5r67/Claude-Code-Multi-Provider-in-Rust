# Design: Status Panel (Phase A observability)

Date: 2026-08-13
Status: Approved design (user granted blanket approval for design and execution).

## Goal

Read-only observability for Big Brother: a `GET /status` JSON endpoint and an
embedded `GET /panel` HTML page showing orchestrator state, budget usage,
sticky conversations, and recent escalations — replacing log-tailing as the
way to see what the proxy is doing.

## Decisions taken

| Question | Decision |
|---|---|
| Refresh model | Page auto-polls `/status` every 3 seconds via inline JS |
| Escalation history | In-memory ring buffer, last 500 entries, reset on restart |
| Packaging | Panel is one self-contained HTML file compiled into the binary via `include_str!`, served on the existing listener |

## Non-goals

- No control endpoints (pause, budget change, sticky clear) — that is Phase B.
- No auth: the listener already binds `127.0.0.1` and both routes are
  read-only; nothing beyond that.
- No persistence: history and counters reset on restart, like all
  orchestrator state.
- No new dependencies. Timestamps are Unix epoch seconds (no chrono); the
  page renders them as local time in JS.

## `GET /status` response

```json
{
  "proxy": {
    "version": "0.1.0",
    "default_provider": "...", "default_model": "...",
    "providers": [
      {"name": "...", "base_url": "...", "auth_style": "bearer", "api_key_present": true}
    ]
  },
  "orchestrator": {
    "enabled": true,
    "local_provider": "qwen", "escalation_provider": "anthropic",
    "escalation_model": "claude-opus-5",
    "sentinel": "<<ESCALATE>>", "fail_mode": "cloud",
    "budget": {"max_per_hour": 50, "used_last_hour": 3, "remaining": 47},
    "sticky_cloud_conversations": 2,
    "escalations": {
      "total_since_start": 17,
      "budget_denied_since_start": 1,
      "recent": [
        {"at_epoch_secs": 1786655052, "trigger": "sentinel",
         "provider": "anthropic", "model": "claude-opus-5",
         "conversation_key_prefix": "a3f19c2e"}
      ]
    }
  }
}
```

- `orchestrator` is JSON `null` when the config section is absent or
  disabled; the rest of the response still renders.
- `recent` is newest-first, at most 500 entries.
- Triggers: `sentinel`, `sticky`, `fail_mode` (granted escalations, counted
  in `total_since_start`) and `budget_denied` (local fallback, counted in
  `budget_denied_since_start` only).
- Privacy: no message content anywhere; conversations appear only as the
  first 8 characters of their SHA-256 key; API keys appear only as the
  boolean `api_key_present`.

## State additions (`src/orchestrator.rs`)

- `EscalationRecord { at_epoch_secs: u64, trigger, provider, model, conversation_key_prefix: Option<String> }`, `serde::Serialize`.
- A `Mutex`-guarded history struct: `VecDeque<EscalationRecord>` capped at
  `ESCALATION_HISTORY_CAP = 500` (oldest evicted), plus running totals
  `total_escalations` and `total_budget_denied` that survive eviction.
- `record_escalation(trigger, provider, model, key: Option<&str>)` — called
  from the proxy's `escalate()`: on a granted reservation with the granted
  trigger; on a denied reservation with `budget_denied` and the local
  provider/model.
- `budget_used_at(now) -> u32` — prune-and-count without reserving (the
  prune loop is shared with `try_reserve_cloud_call_at`).
- `sticky_count() -> usize`.
- `status() -> OrchestratorStatus` — one serializable snapshot struct
  (`enabled`, tiers, sentinel, `fail_mode`, `BudgetStatus`, sticky count,
  `EscalationsStatus` with totals + newest-first `recent`). The HTTP handler
  only ever touches this snapshot, never the internals.
- `config::AuthStyle` and `config::FailMode` gain `serde::Serialize`
  (lowercase, matching their existing `Deserialize` renames).

## Routes (`src/proxy.rs`)

- `GET /status` — builds the JSON above from `AppState` (`env!("CARGO_PKG_VERSION")`,
  config, `orchestrator.as_ref().map(|o| o.status())`). Cannot meaningfully
  fail: all data is in-memory; mutex poisoning panics like the rest of the
  orchestrator. Never touches upstream providers — the panel can never
  trigger model traffic or cost.
- `GET /panel` — serves `include_str!("panel.html")` as `text/html`.

## The page (`src/panel.html`)

One self-contained file (inline CSS + vanilla JS, no external assets, works
offline). Top to bottom:

- Header: "Big Brother" + version + live/unreachable indicator dot.
- Orchestrator card: enabled/disabled, local → escalation tiers, escalation
  model, sentinel, fail mode. When `orchestrator` is null the card reads
  "orchestrator disabled" and the sections below it hide.
- Budget bar: `used_last_hour / max_per_hour` fill, amber at ≥80%, red at 100%,
  remaining count alongside.
- Stat tiles: sticky conversations, escalations since start, budget-denied
  since start.
- Escalation table, newest first: local time, trigger as a color-coded chip
  (sentinel / sticky / fail_mode / budget_denied), provider → model, key
  prefix. Empty state: "No escalations yet."
- Providers table: name, base_url, auth style, key-present check.

JS: `setInterval(fetchStatus, 3000)`; on fetch failure the indicator flips to
"proxy unreachable" and the last data stays visible, grayed, rather than
blanking. Timestamps render via `new Date(at_epoch_secs * 1000)` in the
viewer's timezone.

## Testing

- Unit (`orchestrator.rs`): ring cap at 500 with eviction order; totals
  survive eviction; `budget_used_at` counts without consuming and prunes
  aged entries; `status()` snapshot contents.
- Integration (`tests/proxy_integration.rs`, existing oneshot harness):
  `/panel` returns 200 `text/html` containing a known marker; `/status`
  with orchestrator disabled has `"orchestrator": null`; end-to-end — drive
  one sentinel escalation through wiremock, then `/status` shows
  `used_last_hour == 1`, sticky count 1, and one `recent` entry with trigger
  `"sentinel"` and the configured provider/model.

## Documentation

A short "Status panel" section in `docs/USER_GUIDE.md`: the URL, what it
shows, localhost-only/read-only, history resets on restart.
