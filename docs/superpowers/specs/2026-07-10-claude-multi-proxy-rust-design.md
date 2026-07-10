# Design: `claude-multi-proxy` (Rust rewrite of simple-proxy.py)

Date: 2026-07-10

## Purpose

A local HTTP reverse proxy that routes Claude Code requests to multiple LLM
providers, enabling in-session model switching via a `/model <provider>/<model>`
command. This is an idiomatic Rust rewrite of the original `simple-proxy.py`
(FastAPI/httpx), using **axum**, **reqwest**, **serde**, and **tracing**.

Original source: https://gist.github.com/spideynolove/13785891385ed6916619ebb991b490b9

## Scope decisions

- **Fidelity:** Improve freely — idiomatic Rust, config file, better errors,
  structured logging, tests. May diverge from the original's shape.
- **Config:** TOML config file; API keys resolved from env vars (never inlined).

## Architecture

Single binary on the Tokio async runtime. axum HTTP server bound to a
configurable `host:port` (default `127.0.0.1:8787`). A single shared
`reqwest::Client` (rustls-TLS — no OpenSSL dependency on Windows) forwards
requests to the selected upstream provider. Config is loaded once at startup
into an `Arc` and shared with handlers via axum state.

## Module layout

```
Cargo.toml
config.toml            # sample/default config, checked in
src/
  main.rs              # tracing init, load config, build router, serve
  config.rs            # Config/Provider structs, TOML load, API-key resolution
  model_command.rs     # parse & strip "/model <provider>/<model>" (unit-tested)
  proxy.rs             # POST /v1/messages handler + forwarding, AppState
  error.rs             # AppError enum -> IntoResponse (status + JSON body)
README.md
```

## Configuration (TOML)

```toml
[server]
host = "127.0.0.1"
port = 8787
request_timeout_secs = 300

[default]
provider = "openrouter"
model = "x-ai/grok-code-fast-1"

[providers.deepseek]
base_url = "https://api.deepseek.com/anthropic/v1/messages"
api_key_env = "DEEPSEEK_API_KEY"

[providers.zai]
base_url = "https://api.z.ai/api/anthropic/v1/messages"
api_key_env = "Z_AI_API_KEY"

[providers.kimi]
base_url = "https://api.moonshot.ai/anthropic/v1/messages"
api_key_env = "KIMI_API_KEY"

[providers.openrouter]
# y-router runs on its OWN port — fixes the original self-loop where openrouter
# pointed back at the proxy's own 127.0.0.1:8787.
base_url = "http://localhost:8788/v1/messages"
api_key_env = "OPENROUTER_API_KEY"
```

Config path resolved from the `PROXY_CONFIG` env var (default `./config.toml`).
On startup, log which provider API-key env vars are present vs. missing
(warning, not fatal — a missing key only fails when that provider is actually
routed to).

## Behavior

### `POST /v1/messages`
1. Read and parse the request body as JSON. Invalid JSON -> `400`.
2. Start with `provider = default.provider`, `model = body["model"]` (or
   `default.model` if absent).
3. Run `/model` command detection over the messages. On a match of the form
   `/model <provider>/<model>`: reroute to that provider+model, and strip the
   command text from the message (removing the message entirely if it becomes
   empty).
4. Set `body["model"] = model`.
5. Resolve provider config; inject headers `Authorization: Bearer <key>`,
   `x-api-key: <key>` (y-router compatibility), `Content-Type: application/json`.
6. Forward the (possibly modified) JSON body to the provider `base_url` with the
   configured timeout (default 300s).
7. **Stream the upstream response body straight through** for both streaming and
   non-streaming responses, preserving the upstream status code and
   `content-type` header. (Simpler and more correct than the original's
   parse-and-re-serialize branch; forwards SSE unbuffered.)

### `GET /health`
Returns `{"status":"ok"}`.

## `/model` command parsing

Detect `/model <identifier>` where `identifier` contains a `/`
(`<provider>/<model>`). Handle **both** content shapes:
- `content` is a string (as in the original), and
- `content` is an array of blocks — check text blocks (`{"type":"text","text":...}`).
  Claude Code typically sends block arrays, which the original silently missed.

Only the first matching user message is acted upon. Stripping removes just the
`/model <identifier>` token; if the remaining content is empty, the message is
removed from the array.

## Error handling (`AppError` -> response)

| Condition                     | Status | Body                              |
|-------------------------------|--------|-----------------------------------|
| Body is not valid JSON        | 400    | `{"error": "..."}`                |
| Unknown provider              | 400    | `{"error": "invalid provider ..."}`|
| Missing API-key env var       | 500    | `{"error": "... not set ..."}`    |
| Upstream request/network fail | 502    | `{"error": "..."}`                |

Upstream *HTTP* errors (4xx/5xx returned by the provider) are forwarded through
with their real status and body, and logged at `warn`.

## Logging

`tracing` + `tracing-subscriber` with an `EnvFilter` (default `info`, override
via `RUST_LOG`). Log: startup (bind address, provider key presence), each route
decision (`provider`, `model`, `base_url`), model-switch commands, and upstream
errors.

## Testing

- **Unit — `model_command`:** string-form command; block-array command;
  command-only message removed; non-`provider/model` token ignored; no-command
  passthrough leaves messages untouched.
- **Unit — `config`:** parse a representative TOML string into `Config`.
- **Integration — `proxy`:** `wiremock` mock provider + `tower::ServiceExt::oneshot`
  against the axum router — assert header injection, `model` override, and
  streaming body passthrough.

## Dependencies

Runtime: `tokio` (full), `axum`, `reqwest` (rustls-tls, stream, json),
`serde` (derive), `serde_json`, `toml`, `tracing`, `tracing-subscriber`
(env-filter), `thiserror`, `futures-util`.
Dev: `wiremock`, `tower` (ServiceExt), `http-body-util`.
