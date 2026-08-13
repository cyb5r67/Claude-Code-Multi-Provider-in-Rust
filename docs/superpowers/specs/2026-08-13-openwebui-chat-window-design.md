# Open WebUI Chat Window Through Big Brother — Design

**Date:** 2026-08-13
**Status:** Approved by user (approach and design sections confirmed in session)

## Problem

Big Brother currently serves exactly one client: the Claude Code CLI, speaking the
Anthropic Messages dialect on `POST /v1/messages`. The user wants the option to chat
through a browser chat window instead — specifically Open WebUI, which is already in
the compose stack but bypasses Big Brother entirely (it speaks the OpenAI dialect
straight to LM Studio). The user also wants to control the chat window's behavior from
Big Brother's existing web panel (`/panel`): whether chat traffic goes through the
routing/cascade/escalation pipeline at all, and which model/tier it routes to.

## Chosen approach

Big Brother learns the OpenAI dialect natively (Approach 1 of 3 considered).
Alternatives rejected: a LiteLLM-style translation sidecar (extra container, config in
two places, panel controls would still need proxy changes) and an Open WebUI "pipe"
plugin speaking Anthropic (logic hidden in Open WebUI's database, untestable from this
repo).

## Architecture

Two new endpoints join the existing axum router in `src/proxy.rs`:

- `POST /v1/chat/completions` — OpenAI-dialect chat endpoint (streaming and
  non-streaming).
- `GET /v1/models` — advertises a single virtual model, `big-brother`, so Open WebUI
  has something to select. Deliberately minimal: the panel is the single source of
  truth for routing; Open WebUI's own model picker must not be able to fight it.

A new module `src/openai_compat.rs` owns dialect translation in both directions:

- **Inbound:** OpenAI chat request → internal Anthropic Messages request. Maps
  `messages` (including the `system` role → Anthropic `system` field), `temperature`,
  `max_tokens`, `stream`, and drops/defaults unsupported fields.
- **Outbound:** Anthropic response → OpenAI `chat.completion` object; for streaming,
  Anthropic SSE deltas → OpenAI `chat.completion.chunk` SSE events, terminated by
  `data: [DONE]`.

After inbound translation, chat requests enter the **same pipeline** Claude Code
traffic uses — routing, local-first cascade, `<<ESCALATE>>` sentinel scanning
(`src/stream.rs`), sticky per-conversation tiers, hourly cloud budget, `fail_mode`.
There is no parallel pipeline.

## Chat settings and the panel

New `ChatSettings` state in `AppState`, behind a lock:

- `pipeline_enabled: bool` — pipeline vs. raw passthrough.
- `model_override: String` — `"cascade"` or a specific `provider/model`.

Defaults come from a new `[chat]` section in `config.toml`. Panel edits are persisted
to a small JSON state file stored next to the config and reloaded at startup, so
choices survive proxy/container restarts. Config.toml remains the fallback when the
state file is absent or unreadable.

Panel additions (`src/panel.html`): a "Chat" card with a pipeline on/off toggle and a
routing-target dropdown (Cascade + every configured `provider/model`). Backed by two
new endpoints: `GET /chat/settings` and `PUT /chat/settings`.

**Security note:** the panel is unauthenticated; `PUT /chat/settings` is its first
mutating endpoint. This remains acceptable **only** because the service binds to
127.0.0.1 (host port mapping `127.0.0.1:8787`). If the bind address is ever widened,
these endpoints need auth first.

## Routing semantics

| Panel state | Behavior |
|---|---|
| Pipeline ON, target = Cascade | Identical to Claude Code traffic: local model first, escalate to cloud on sentinel. Cloud budget is **shared** with Claude Code traffic (one user, one budget). |
| Pipeline ON, target = `provider/model` | Route directly to that provider/model; no cascade, no sentinel scanning. |
| Pipeline OFF | Forward the OpenAI request to LM Studio's `/v1/chat/completions` with exactly one change: the `model` field is rewritten from the virtual `big-brother` to the configured local model id. Response streamed back untouched. Otherwise equivalent to today's direct wiring, one hop later. |

Sticky-tier tracking reuses the existing conversation-hash mechanism, keyed off the
chat message history.

## Docker changes

- `open-webui` joins the `proxy` network (keeps `webui`).
- `OPENAI_API_BASE_URL` becomes `http://big-brother:8787/v1`; the dummy API key stays.

Nothing else in the stack moves. Grafana/Prometheus wiring is untouched.

## Error handling

- Everything returned to Open WebUI is OpenAI-shaped: `{"error": {"message", "type",
  "code"}}`.
- Pipeline failures follow existing `fail_mode` semantics (fall through to cloud, or
  error).
- Passthrough failures (LM Studio unreachable) → 502 with OpenAI-shaped body.
- Malformed chat requests → 400 before touching the pipeline.
- Corrupt or missing settings state file → fall back to `config.toml` defaults, log a
  warning, never fail startup.

## Observability

Existing Prometheus metrics gain a label distinguishing chat traffic from Claude Code
traffic (e.g. `client="chat"` vs `client="code"`), so the Grafana dashboard can split
them.

## Testing

- **Unit:** translator both directions, including streaming (Anthropic SSE deltas →
  OpenAI chunks) and interaction with `SseSentinelScanner`; settings serialization
  and fallback behavior.
- **Integration (wiremock):** cascade escalation via the chat endpoint; direct-model
  override; passthrough mode; settings persistence round-trip (PUT → file → restart →
  reload); `/v1/models` shape.

## Out of scope

- Authentication for the panel or the new endpoints.
- Exposing multiple models to Open WebUI's model picker.
- Tool/function calling, images, or multimodal content through the chat endpoint.
- Per-conversation (rather than global) chat settings.
