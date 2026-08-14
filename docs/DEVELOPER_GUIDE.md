# Developer Guide

For engineers building on, operating, or reviewing Big Brother. Assumes you
can read Rust but know nothing about this codebase.

Companion documents: [Architecture](ARCHITECTURE.md) has the diagrams and
request-flow walkthroughs; [User Guide](USER_GUIDE.md) is the behavior
reference. This document covers how to work *in* the code.

---

## Orientation

Big Brother is a single-process HTTP reverse proxy. It accepts LLM requests in
two dialects, decides where they should go, and forwards them upstream —
streaming responses back untouched wherever possible.

There is no database, no message queue, no background job runner, and no
persistent state beyond one small JSON settings file. Everything shares an
`Arc`-cloned `AppState`. If you're looking for complexity, it lives in three
places: routing decisions (`proxy.rs`), the escalation state machine
(`orchestrator.rs` + `stream.rs`), and dialect translation
(`openai_compat.rs`).

---

## First build

```sh
cargo build              # compile
cargo test               # 134 tests: unit (in-module) + integration (tests/)
cargo clippy --all-targets   # lint — keep this clean
cargo fmt                # format before committing
cargo run                # runs with ./config.toml
```

`cargo test` needs no network, no Docker, and no API keys — upstream providers
are mocked with `wiremock`.

Logging is `tracing` via `RUST_LOG` (default `info`). `RUST_LOG=debug` is
usually what you want while developing; every routing decision is logged at
`info`.

---

## Codebase map

| File | Lines | Responsibility |
|------|-------|----------------|
| `src/main.rs` | 48 | Entrypoint: tracing setup, config load, bind, serve |
| `src/lib.rs` | 72 | `build_state()`, startup diagnostics |
| `src/config.rs` | 448 | TOML model, loading, API-key resolution from env |
| `src/proxy.rs` | 649 | Router, `resolve_route`, `/v1/messages`, cascade, `forward` |
| `src/orchestrator.rs` | 432 | Sticky tiers, sliding budget, escalation history |
| `src/stream.rs` | 298 | Sentinel detection over SSE and JSON responses |
| `src/openai_compat.rs` | 417 | OpenAI ⇄ Anthropic translation, incl. streaming |
| `src/chat_proxy.rs` | 281 | OpenAI-dialect routes and chat settings endpoints |
| `src/chat_settings.rs` | 113 | Runtime-editable settings, persisted to JSON |
| `src/model_command.rs` | 307 | Legacy `/model` text-command parsing |
| `src/metrics.rs` | 231 | Prometheus registry and instruments |
| `src/error.rs` | 108 | `AppError` → HTTP status + JSON body |
| `src/panel.html` | 209 | The embedded status panel (no external assets) |
| `tests/proxy_integration.rs` | 1056 | End-to-end tests for the Anthropic path |
| `tests/chat_integration.rs` | 397 | End-to-end tests for the OpenAI path |

Line counts include in-file `#[cfg(test)]` modules, which is most of the bulk
in `config.rs`, `model_command.rs`, and `orchestrator.rs`.

**Rule of thumb for placement:** routing decisions go in `proxy.rs`; anything
that reasons about *response content* goes in `stream.rs`; anything that
translates between wire formats goes in `openai_compat.rs`. Handlers should
stay thin.

---

## The two request paths

**`POST /v1/messages`** (Anthropic dialect, from Claude Code) →
`resolve_route` picks a provider → if the route came from defaults *and* an
orchestrator is configured, run the cascade; otherwise forward directly.

**`POST /v1/chat/completions`** (OpenAI dialect, from chat clients) → check
`ChatSettings` → either passthrough to the local OpenAI endpoint, or translate
to the Anthropic dialect and enter the *same* cascade/forward machinery, then
translate the response back.

The second path deliberately reuses the first path's internals rather than
duplicating them. If you add routing behavior, both front doors should inherit
it automatically — and if they don't, that's a bug worth fixing at the shared
layer instead of patching twice.

Full sequence diagrams: [Architecture §4 and §8](ARCHITECTURE.md).

---

## Load-bearing invariants

Break one of these and something subtle stops working. Each has tests behind
it; if you're changing one deliberately, update the tests and this list.

**Streaming is never re-serialized on the Anthropic path.** Upstream bytes are
forwarded verbatim. `SseTextScanner` buffers *raw chunks* while it looks for
the sentinel and releases them unmodified. Don't parse-and-rebuild SSE frames
here — it would add latency and risk mangling content.

**The sentinel only counts as the first token.** A sentinel appearing
mid-response is ordinary text, not an escalation signal. This is a
prompt-injection defense: content the model is summarizing must never be able
to trigger a cloud call.

**A non-text first content block rules out the sentinel.** If a response opens
with `tool_use` or `thinking`, the verdict is immediately `Clean` — otherwise
tool-using local models would stall waiting for text that never comes.

**The orchestrator engages only for `RouteSource::Default`.** An explicit
provider choice (via `/model` or a `provider/model` field) is a human
decision and is never second-guessed by the cascade.

**Conversation identity is the SHA-256 of the first user message's text.**
Claude Code resends full history each turn, which makes this stable for the
life of a conversation. No text-bearing user message means no key, which means
no stickiness — callers must handle `None`.

**The cloud budget is one sliding hour of reservations, shared by both front
doors.** One user, one bill, one bucket. A denied reservation consumes nothing.

**Read-only endpoints stay read-only.** `/status`, `/panel`, `/metrics`, and
`/health` must never call an upstream provider. `PUT /chat/settings` is the
*only* mutating endpoint, and it is acceptable only because the service binds
to localhost. Adding auth is a prerequisite for widening the bind address.

**Chat-route errors are always OpenAI-shaped.** Whatever fails internally, the
client sees `{"error": {"message", "type", "code"}}`. Clients parse this.

**`GET /v1/models` advertises exactly one virtual model.** The panel is the
source of truth for routing; letting the client's model picker compete with it
produces confusing behavior.

**Metric names and label sets are a public interface.** The provisioned
Grafana dashboard queries them by name. Adding a label to an existing series
breaks those queries — add a new series instead, which is why chat traffic got
`bb_chat_requests_total` rather than a label on `bb_requests_total`.

**`Config` is immutable after startup** (`Arc<Config>`). The only mutable
runtime state is `ChatSettings` behind an `RwLock`, plus the orchestrator's
own interior-mutable state.

**Take orchestrator locks one at a time.** `Orchestrator::status()` deliberately
scopes each lock separately; holding two at once would create an undocumented
lock-order invariant that a future caller could deadlock against.

---

## Testing

**Unit tests** live in `#[cfg(test)] mod tests` at the bottom of the module
they cover. Pure logic — parsing, translation, sentinel verdicts, budget
arithmetic — belongs here.

**Integration tests** in `tests/` drive the real axum router via
`tower::ServiceExt::oneshot` against `wiremock` upstreams. Anything involving
routing decisions, headers, status codes, or streaming belongs here.

### Conventions that will bite you if ignored

**Use a unique env-var name per test.** Tests run as parallel threads in one
process, so shared env-var names race. Existing tests use prefixes like
`IT_PRIMARY_KEY` and `CHAT_IT_KEY`. Where a shared name is unavoidable (e.g.
`PROXY_CONFIG`), assert all branches inside a single sequential test.

**Mock SSE with `set_body_raw(body, "text/event-stream")`, not
`set_body_string`.** `set_body_string` sets its own content type, so the proxy
takes the JSON branch and your streaming test fails with a confusing parse
error.

**Give each test its own settings state file.** `chat_integration.rs` derives
temp paths from the process id and removes them afterwards; reuse that helper
rather than a fixed filename.

**Use forward slashes in TOML paths.** Windows backslashes need escaping
inside TOML strings; forward slashes work on both platforms.

### What good coverage looks like here

For a new routing behavior: one unit test for the decision function, one
integration test proving the right upstream was called with the right body,
and — if it touches streaming — one test with a chunk boundary in an awkward
place. `openai_compat.rs` has an example that splits an SSE line mid-frame.

---

## Common changes

### Add a provider

Providers are configuration, not code, as long as the endpoint speaks the
Anthropic Messages API:

```toml
[providers.myprovider]
base_url = "https://api.example.com/anthropic/v1/messages"  # full endpoint URL
api_key_env = "MYPROVIDER_API_KEY"
model = "their-default-model"        # optional; enables bare-name selection
auth_style = "bearer"                # or "anthropic"
```

Two gotchas: `base_url` is the **complete endpoint path**, not a host root; and
`api_key_env` must name a variable holding a **non-empty** value even for local
servers that ignore keys.

A provider speaking a different dialect needs translation code — follow the
pattern in `openai_compat.rs` rather than special-casing inside `forward`.

### Add a config field

1. Add the field to the relevant struct in `config.rs` with a `#[serde(default
   = "...")]` if it's optional.
2. Add a parse test proving both the default and an explicit override.
3. Thread it through `build_state` if it needs runtime state.

Keep new sections optional (`Option<T>`) so existing configs keep working.

### Add a metric

Add the instrument in `Metrics::new`, register it, add it to the struct
initializer, and add a render test using the `has_series` helper. Never assert
on label *order* — the encoder decides it. If a dashboard should show it, edit
`docker/grafana/dashboards/big-brother.json`.

### Extend the chat translator

Inbound and outbound translation are separate functions with separate tests;
change them independently. If you add support for a field, add a rejection or
pass-through test for the shapes you *don't* support — silent dropping is how
clients end up confused.

---

## Configuration and runtime state

| Section | Required | Purpose |
|---------|----------|---------|
| `[server]` | No (defaults) | Bind host/port, upstream timeout |
| `[default]` | **Yes** | Fallback provider and model |
| `[providers.<name>]` | **Yes** (≥1) | Upstream endpoints and key env vars |
| `[orchestrator]` | No | Local-first cascade; absent means plain routing |
| `[chat]` | No | OpenAI front door; absent means chat routes 404 |

Config path resolution: CLI argument → `PROXY_CONFIG` env var →
`./config.toml`. The resolved path is logged at startup.

**Runtime state** is one JSON file (`[chat] state_file`) holding the
panel-edited chat settings. It's written on every successful `PUT`, read once
at startup, and a corrupt or missing file falls back to config defaults with a
warning — never a startup failure. In Docker it lives on the
`big-brother-data` volume. Everything else — budget window, sticky map,
escalation history — is in-memory and resets on restart, by design.

---

## Observability

| Metric | Type | Labels |
|--------|------|--------|
| `bb_requests_total` | counter | `provider`, `outcome` (`ok`/`upstream_error`/`transport_error`) |
| `bb_request_duration_seconds` | histogram | `provider` |
| `bb_tier_requests_total` | counter | `tier` (`local`/`cloud`/`static`) |
| `bb_chat_requests_total` | counter | `mode` (`pipeline`/`passthrough`) |
| `bb_escalations_total` | counter | `trigger` (`sentinel`/`sticky`/`fail_mode`) |
| `bb_budget_denied_total` | counter | — |
| `bb_cloud_budget_used` | gauge | — |
| `bb_cloud_budget_max` | gauge | — |
| `bb_sticky_conversations` | gauge | — |

Gauges are refreshed from live state when `/metrics` is scraped.

Note that a budget-denied fallback dispatches `local` twice in
`bb_tier_requests_total` — once for the original attempt, once for the local
answer that replaces the denied escalation. That's intentional; it counts
dispatches, not conversations.

`GET /status` returns the same picture as JSON, including current chat
settings, and is the easiest thing to script against.

### Runbook

| Symptom | Where to look |
|---------|---------------|
| All requests 502 | `bb_requests_total{outcome="transport_error"}` — upstream unreachable, or a `base_url` that isn't a full endpoint path |
| All requests 500 | Missing API key; the startup log lists every key as present or absent |
| Escalations never happen | Orchestrator disabled/absent, or the local model isn't emitting the sentinel first — check a raw response |
| Escalations happen constantly | Local model over-signaling; inspect `bb_escalations_total{trigger}` and the panel's recent-escalation list |
| Budget exhausted early | `bb_cloud_budget_used` vs `_max`; remember the budget is shared with chat traffic |
| Chat window errors, CLI fine | `[chat]` section missing, or `passthrough_url` pointing at an Anthropic-dialect path instead of `/v1/chat/completions` |

Every escalation writes one greppable audit line at `info` with its trigger,
provider, and model.

---

## Docker and release

The stack is five services: `big-brother`, `open-webui`, `docexport`,
`prometheus`, `grafana`. Networks are deliberately segmented — Grafana reaches
only Prometheus, never the proxy; `docexport` sits on the `webui` network
only. All host ports bind to `127.0.0.1`.

`docexport` is a Python sidecar with its own tests and its own
[README](../docexport/README.md); it is not part of the proxy's request path.
It exists because Open WebUI's Action functions can't install the system
binaries (pandoc, the WeasyPrint rendering stack) that document conversion
needs.

Inside the container the proxy binds `0.0.0.0` (Docker port mapping can't reach
a container's loopback); host-side exposure stays localhost-only through the
port mapping. Don't "fix" the `0.0.0.0` bind.

```sh
docker compose up -d --build       # build and start
docker compose restart big-brother # apply docker/config.toml edits
docker compose up -d               # apply .env changes (needs recreate)
docker compose logs -f big-brother
docker compose config              # validate compose syntax
```

Before tagging a release: `cargo test`, `cargo clippy --all-targets`,
`cargo fmt --check`, and `docker compose config`.

---

## Conventions

**Comments explain constraints, not mechanics.** The existing code comments
things a reader couldn't infer — why the sentinel must be first, why each lock
is scoped separately, why `0.0.0.0` is correct in a container. Don't narrate
what the next line does.

**Doc comments on public items.** Modules get `//!`, public functions and
types get `///`.

**Errors carry context.** `AppError` variants name the provider and env var
involved; keep that up when adding variants.

**Commits are small and green.** Each commit compiles and passes tests.

**Design docs precede substantial features.** Specs live in
`docs/superpowers/specs/`, implementation plans in `docs/superpowers/plans/`,
both dated. They're historical records — don't retro-edit them when behavior
changes; update the living docs (`README`, `USER_GUIDE`, `ARCHITECTURE`, this
file) instead.

---

## Security posture

- Binds to `127.0.0.1` by default; the panel has **no authentication**.
- `PUT /chat/settings` is the only mutating endpoint and is safe only under
  that assumption. **Adding auth is a hard prerequisite** for exposing the
  service beyond localhost.
- API keys are never written to config files — providers name an environment
  variable, resolved per request.
- No message content reaches the panel, the status JSON, or the escalation
  history; conversations appear only as 8-character hash prefixes, keys only
  as present/missing.
- Upstream error bodies are logged truncated to 2 KB.
- The sentinel-must-be-first rule is a prompt-injection defense; treat it as
  security-relevant, not a style choice.

---

## Known gaps

Worth knowing before you plan work:

- The chat endpoint supports text conversation only — no tool calling, no
  images, no multimodal content.
- Chat settings are global, not per-conversation.
- Escalation history and budget state are in-memory; a restart clears them.
- The `[chat]` passthrough target is a single fixed endpoint.
- Panel authentication does not exist.
