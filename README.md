# claude-multi-proxy

A local HTTP reverse proxy that routes [Claude Code](https://claude.com/claude-code)
requests to multiple LLM providers, with in-session model switching via a
`/model <provider>/<model>` command.

This is a Rust rewrite (axum + reqwest + serde + tracing) of the original
[`simple-proxy.py`](https://gist.github.com/spideynolove/13785891385ed6916619ebb991b490b9)
(FastAPI/httpx).

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for process-flow and module
diagrams (Mermaid).

## How it works

- Listens on `127.0.0.1:8787` (configurable).
- `POST /v1/messages` — forwards the Claude Code request to the selected provider.
  - Provider/model default to the values in `config.toml`, or the request body's
    own `model`.
  - A `/model <provider>/<model>` command in a user message reroutes the request
    and is stripped from the text before forwarding, so the upstream model never
    sees the command. Both plain-string and content-block message shapes are
    handled.
  - Responses stream straight through (SSE and JSON alike), preserving the
    upstream status and content type.
- `GET /health` — returns `{"status":"ok"}`.

## Configuration

Providers, defaults, and server settings live in `config.toml`. API keys are
**not** stored in the file — each provider names an environment variable holding
its key. See the checked-in [`config.toml`](config.toml) for the full example.

The config path is taken from `PROXY_CONFIG` (default `./config.toml`).

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

# 2. Run the proxy
cargo run --release

# 3. Point Claude Code at it
export ANTHROPIC_BASE_URL="http://localhost:8787"
export ANTHROPIC_API_KEY="dummy"
claude
```

Switch providers mid-session:

```
/model deepseek/deepseek-chat
```

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
| `src/model_command.rs`| Parse & strip `/model provider/model` commands        |
| `src/proxy.rs`        | Router, `/v1/messages` forwarding, `/health`          |
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
