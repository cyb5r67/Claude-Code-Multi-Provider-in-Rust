# Big Brother — On-the-Fly Model Switching

A local HTTP reverse proxy that routes [Claude Code](https://claude.com/claude-code)
requests to multiple LLM providers, with in-session model switching via a
`/model <provider>/<model>` command.

This is a Rust rewrite (axum + reqwest + serde + tracing) of the original
[`simple-proxy.py`](https://gist.github.com/spideynolove/13785891385ed6916619ebb991b490b9)
(FastAPI/httpx).

## Documentation

| Document | For | Contents |
|----------|-----|----------|
| [Getting Started](docs/GETTING_STARTED.md) | New users | Five-minute setup, both front doors, the control panel, everyday tasks, troubleshooting |
| [User Guide](docs/USER_GUIDE.md) | Users | Full configuration reference, model-switching semantics, the orchestrator, LM Studio examples |
| [Architecture](docs/ARCHITECTURE.md) | Engineers | Process-flow and module diagrams (Mermaid) |
| [Developer Guide](docs/DEVELOPER_GUIDE.md) | Contributors, operators | Codebase map, invariants, testing conventions, observability, runbook |
| [Product Brief](docs/PRODUCT_BRIEF.md) | Marketing, non-technical | Positioning, capabilities, messaging, claims discipline |

## How it works

- Listens on `127.0.0.1:8787` (configurable).
- `POST /v1/messages` — forwards the Claude Code request to the selected provider.
  - Provider/model default to the values in `config.toml`, or the request body's
    own `model`.
  - A `model` field of the form `<provider>/<model>` (which is what Claude
    Code's built-in `/model` command sends) selects that provider directly;
    everything after the first `/` is forwarded as the model id. A bare
    provider name selects that provider with its configured default `model`.
    Slash-containing ids whose prefix is *not* a configured provider (e.g.
    openrouter's `x-ai/grok-code-fast-1`) pass through to the default provider
    unchanged.
  - Legacy: a `/model <provider>/<model>` command appearing as user message
    *text* also reroutes the request and is stripped before forwarding. Both
    plain-string and content-block message shapes are handled.
  - Responses stream straight through (SSE and JSON alike), preserving the
    upstream status and content type.
- `POST /v1/chat/completions` + `GET /v1/models` — an OpenAI-dialect front
  door for chat clients (Open WebUI in the bundled stack). Requests are
  translated to the Anthropic dialect and enter the same routing/cascade
  pipeline as Claude Code traffic, or pass straight through to the local
  OpenAI endpoint — controlled at runtime from the panel's Chat card
  (`GET`/`PUT /chat/settings`, persisted across restarts). Requires a
  `[chat]` section in `config.toml`.
- `GET /health` — returns `{"status":"ok"}`.
- Optional **hierarchical orchestrator**: answer conversations with a local
  model first and transparently escalate to a cloud model when the local tier
  signals a task is beyond it. See the
  [user guide](docs/USER_GUIDE.md#hierarchical-orchestrator-local-first-with-cloud-escalation).
- `GET /panel` — a read-only status page (orchestrator state, budget, recent
  escalations); `GET /status` serves the same data as JSON.
- Runs natively (`cargo run`) or as a Docker Compose stack bundled with
  [Open WebUI](https://github.com/open-webui/open-webui) — see Usage.

## Configuration

Providers, defaults, and server settings live in `config.toml`. API keys are
**not** stored in the file — each provider names an environment variable holding
its key. See the checked-in [`config.toml`](config.toml) for the full example.

The config path is resolved in order of precedence: first CLI argument
(`cargo run -- my-config.toml`), then the `PROXY_CONFIG` env var, then
`./config.toml`. The path actually loaded is logged at startup.

> **Note on the `openrouter` provider:** the original Python proxy pointed
> `openrouter` at `http://localhost:8787/v1/messages` — the proxy's *own* address —
> which loops back into itself. Run your y-router instance on a separate port
> (the sample config uses `8788`).

## Usage

```sh
# 1. Set the API keys your providers need
export DEEPSEEK_API_KEY=...
export OPENROUTER_API_KEY=...
# (etc. — see config.toml)

# 2. Run the proxy (optionally naming a config file)
cargo run --release -- my-config.toml

# 3. Point Claude Code at it
export ANTHROPIC_BASE_URL="http://localhost:8787"
export ANTHROPIC_API_KEY="dummy"
claude
```

### Or with Docker

```sh
copy .env.example .env   # then fill in your keys (cp on Linux/macOS)
docker compose up -d --build
```

The proxy listens on <http://localhost:8787> (status panel at `/panel`) and
[Open WebUI](https://github.com/open-webui/open-webui) on
<http://localhost:3000> as a browser chat window, routed through the proxy and
controlled from the panel's Chat card.
Edit `docker/config.toml` and `docker compose restart big-brother` to apply
config changes. Claude Code connects exactly as above.
Prometheus runs at <http://localhost:9090> and a pre-provisioned Grafana
dashboard at <http://localhost:3001> (login `admin` /
`GRAFANA_ADMIN_PASSWORD`).

Switch providers mid-session:

```
/model deepseek/deepseek-chat    # provider + model
/model deepseek                  # bare provider name -> its configured default model
```

See the [user guide](docs/USER_GUIDE.md#switching-providers-and-models) for the
full semantics (multi-slash model ids, pass-through of non-provider prefixes).

## Development

```sh
cargo build      # compile
cargo test       # unit tests (config, /model parsing) + integration tests
cargo run        # run with ./config.toml
```

Logging is controlled by `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug`.

## Layout

| File                  | Responsibility                                        |
|-----------------------|-------------------------------------------------------|
| `src/main.rs`         | Entrypoint: tracing, config load, bind + serve        |
| `src/lib.rs`          | State construction, startup key-presence logging      |
| `src/config.rs`       | TOML config model, loading, API-key resolution        |
| `src/model_command.rs`| Parse & strip legacy `/model` text commands           |
| `src/proxy.rs`        | Router, `/v1/messages` forwarding, `/health`          |
| `src/chat_proxy.rs`   | OpenAI-dialect chat routes (`/v1/chat/completions`)   |
| `src/openai_compat.rs`| OpenAI ⇄ Anthropic dialect translation (JSON + SSE)  |
| `src/chat_settings.rs`| Panel-editable chat settings, persisted to disk       |
| `src/orchestrator.rs` | Escalation state: sticky map, budget, history      |
| `src/stream.rs`       | Sentinel detection over SSE/JSON responses          |
| `src/panel.html`      | Embedded status panel served at `/panel`            |
| `src/error.rs`        | `AppError` → HTTP status + JSON error body            |

## Credits

This project is a Rust port of
[`simple-proxy.py`](https://gist.github.com/spideynolove/13785891385ed6916619ebb991b490b9)
by **spideynolove**. The original is a FastAPI/httpx reverse proxy for routing
Claude Code requests to multiple LLM providers; this project reimplements that
functionality in Rust (axum + reqwest + serde + tracing) with additional
changes. Credit for the original design and concept goes to the original author.
See [NOTICE](NOTICE) for attribution details.

## License

Copyright 2026 Cyb5r LLC.

Licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE)
for attributions.
