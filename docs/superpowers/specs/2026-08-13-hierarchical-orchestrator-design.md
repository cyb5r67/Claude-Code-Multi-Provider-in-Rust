# Design: Hierarchical Orchestrator (local Qwen + cloud escalation)

Date: 2026-08-13
Status: Approved design; Phase 1 to be planned for implementation, Phase 2 specified at architecture level only.

## Goal

Evolve Big Brother from a passive, human-driven router into a two-tier
orchestrator:

- **Tier 1 (local):** Qwen 3.6 27B served by LM Studio. Fast, free, private.
  Handles every conversation by default.
- **Tier 2 (cloud):** a foundational model with deep reasoning. Direct
  Anthropic API support with `claude-opus-5` as the default target; the
  escalation target remains a config key so any configured provider can fill
  the role.

The local model decides when a task exceeds its capability and the proxy
transparently escalates. Claude Code (the client) never knows which tier
answered.

## Non-goals

- No change to the existing static routing (`/model` commands, model-field
  routing, per-provider defaults). It remains the behavior when the
  orchestrator is disabled, and the explicit `/model` override always wins.
- Phase 1 does not attempt scoped delegation (cloud-as-sub-agent); that is
  Phase 2, whose detailed design is deferred until Phase 1 ships.
- No persistence: sticky state and budget counters are in-memory and reset on
  restart.

## Decisions taken

| Question | Decision |
|---|---|
| Scope | Both phases in one spec; only Phase 1 planned for implementation now |
| Cloud tier | Direct Anthropic support (`claude-opus-5` default) + configurable provider fallback |
| Escalation decision | Self-escalation cascade: Qwen attempts the request and signals when it cannot handle it |
| Phase 1 signal | Leading sentinel token (`<<ESCALATE>>` as first output token) |
| Phase 2 signal | Injected tools: `consult_expert(task, context)` and `handoff()` |
| Post-escalation | Sticky per conversation until the conversation ends |
| Guards | Hourly cloud-request budget cap + audit log line per escalation, both from day one |

## Phase 1 — Sentinel cascade

### Mechanism

1. A request arrives at `POST /v1/messages`.
2. If it carries an explicit `/model` provider selection → existing static
   routing. Done.
3. If the orchestrator is disabled → existing behavior. Done.
4. Sticky-map lookup on the conversation key (hash of the earliest
   messages). Already `Cloud` → route directly to the escalation provider
   (budget-checked), skipping the local attempt.
5. Otherwise append a short sentinel instruction to the system prompt and
   forward to the local provider:
   > "If this task is beyond your capability, output `<<ESCALATE>>` as your
   > very first token and nothing else."
6. Buffer the response's first output chunks only:
   - **No sentinel** (first bytes rule it out): release the buffer and stream
     the remainder through untouched.
   - **Sentinel confirmed:** drop the local stream, strip the injected
     instruction, replay the original payload to the escalation provider,
     mark the conversation sticky-`Cloud`, write the audit line.

### Components

| Component | Change |
|---|---|
| `src/config.rs` | New `[orchestrator]` section: `enabled`, `local_provider`, `escalation_provider`, `escalation_model`, `sentinel` (default `<<ESCALATE>>`), `max_cloud_requests_per_hour`, `fail_mode` (`"cloud"` \| `"error"`, default `"cloud"`). `Provider` gains `auth_style = "bearer" \| "anthropic"`; the `anthropic` style sends `x-api-key` + `anthropic-version: 2023-06-01` (no Bearer header). |
| `src/orchestrator.rs` (new) | Escalation decision, sticky conversation map, sliding-window budget counter, audit logging. |
| `src/stream.rs` (new) | First-token sentinel detection over SSE and plain-JSON responses, tolerant of sentinels split across chunk boundaries; buffer-and-release passthrough. |
| `src/proxy.rs` | `messages_proxy` grows the cascade branch. Static routing path is untouched. |

### Error handling

- **Local tier unreachable / first-token timeout:** per `fail_mode` —
  `"cloud"` escalates (budget permitting), `"error"` passes the 502 through
  as today. Default `"cloud"`.
- **Cloud unreachable after escalation:** forward the upstream error
  status/body unchanged (existing behavior). The conversation stays sticky so
  the next turn retries cloud rather than burning a doomed local attempt.
- **Budget exhausted but Qwen escalated:** re-run locally without the
  sentinel instruction plus a one-line "escalation unavailable, do your best"
  note; log a warning. The user always gets an answer.
- **Sentinel mid-stream:** treated as ordinary content, never an escalation.
  Only the very first output token counts. This prevents file/tool content in
  the conversation from triggering escalations via prompt injection; the
  budget cap bounds the damage if Qwen is tricked into emitting it first.
- **Non-streaming requests:** identical logic applied to the buffered JSON
  body's first text block instead of SSE chunks.

### Guards and observability

- **Budget cap:** `max_cloud_requests_per_hour`, sliding window, in-memory.
  When exceeded, fall back to local and log a warning.
- **Audit log:** one `info` line per escalation: trigger (`sentinel` /
  `sticky` / `fail_mode`), provider, model, and usage tokens when available.
  Request bodies are logged only at `debug`.

### Testing

- **Unit:** sticky-map and budget-window logic as pure functions; sentinel
  detection across chunk boundaries (`<<ESC` / `ALATE>>`); sentinel
  instruction injection and stripping round-trip.
- **Integration (wiremock):** sentinel from local mock → cloud mock receives
  the original payload with `anthropic-version` header; no sentinel → cloud
  never called; second turn of an escalated conversation skips the local
  mock; tripped budget cap → local fallback; explicit `/model` bypasses
  orchestration.
- **Manual acceptance:** Claude Code pointed at Big Brother with
  orchestration enabled — a trivial prompt is answered by Qwen with zero
  cloud calls in the audit log; a hard reasoning prompt is transparently
  answered by the cloud tier; no client-visible difference besides latency.

## Phase 2 — Cloud model as sub-agent (architecture only)

The signal mechanism swaps from the sentinel to injected tools; Phase 1's
stream-inspection machinery is the foundation.

1. **Tool injection.** The proxy appends two synthetic tools to requests
   forwarded to Qwen: `consult_expert(task, context)` ("delegate a hard
   sub-problem to a much stronger reasoning model") and `handoff()` ("this
   whole request needs the stronger model" — the Phase 1 cascade expressed as
   a tool call).
2. **Interception.** When Qwen emits a `tool_use` block naming a synthetic
   tool, the proxy does not forward it to the client. For `consult_expert` it
   builds a fresh Anthropic Messages request to the escalation provider: a
   sub-agent system prompt, the task Qwen composed, and the context Qwen
   passed (optionally auto-attaching conversation history).
3. **Cloud capabilities.** The sub-agent call is a normal Messages request,
   so the big model brings its own features — adaptive thinking is on by
   default on `claude-opus-5`, and the proxy may enable server-side tools
   (e.g. web search) on escalated calls.
4. **Resumption.** The cloud answer returns to Qwen as a `tool_result`; Qwen
   synthesizes and answers. The client sees one coherent assistant stream.
5. **Sub-agent sessions.** The proxy keeps the cloud conversation alive under
   an ID so Qwen can ask follow-ups without resending everything.

**Precondition:** Phase 2 depends on Qwen 3.6 27B emitting well-formed
`tool_use` blocks through the LM Studio Anthropic-compat shim, which the
troubleshooting docs already flag as incomplete. Validate tool-calling
reliability before detailed Phase 2 design; the sentinel path remains as a
fallback if it proves flaky.

## Risks

- **First-token latency:** the cascade adds no classifier round-trip, but
  escalated requests pay the local time-to-first-token before the cloud call
  starts. Accepted trade-off of the cascade approach.
- **Sentinel compliance:** Qwen may not reliably emit the sentinel first (or
  may over-emit it). Tunable via the injected instruction text; the audit log
  makes miscalibration visible; `/model` remains the manual escape hatch.
- **Shim fidelity:** LM Studio's Anthropic compatibility is incomplete;
  Phase 1 deliberately avoids depending on its tool-calling.
