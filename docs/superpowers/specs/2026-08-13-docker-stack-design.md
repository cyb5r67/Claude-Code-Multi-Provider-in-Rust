# Design: Docker Stack

Date: 2026-08-13
Status: Approved (user approved the design and delegated execution end to end).

## Goal

Run the Big Brother platform under Docker Compose: the proxy built from a
multi-stage Rust Dockerfile plus the official Open WebUI image pre-wired to
the LAN LM Studio host. `docker compose up -d` brings up the whole control
plane; Claude Code on the host connects to `http://localhost:8787` exactly as
before.

## Fixed constraints

- **LM Studio is not containerized.** It is a GPU desktop app on a LAN
  machine; containers reach it over the network by IP.
- **Claude Code stays on the host**, pointing at the published proxy port.
- **No Rust source changes.** This is packaging only.

## Decisions taken

| Question | Decision |
|---|---|
| Stack contents | Big Brother + Open WebUI (no y-router service) |
| Container bind | Proxy binds `0.0.0.0` inside the container (a `127.0.0.1` bind would be unreachable through Docker's port mapping) |
| Host exposure | Both services publish to `127.0.0.1` only (`127.0.0.1:8787:8787`, `127.0.0.1:3000:8080`), preserving the panel's localhost-only/no-auth posture |
| Config strategy | A separate `docker/config.toml` mounted read-only — edits need `docker compose restart`, not a rebuild |
| Secrets | `env_file: .env` (gitignored); `.env.example` documents every variable |

## Files

| File | Contents |
|---|---|
| `Dockerfile` | Stage 1 `rust:1-slim`: copy manifest + sources, `cargo build --release`. Stage 2 `debian:bookworm-slim`: `apt-get install ca-certificates curl` (CA store for rustls HTTPS; curl for the healthcheck), non-root `app` user, binary at `/usr/local/bin/big-brother`, `CMD ["big-brother", "/app/config/config.toml"]` |
| `.dockerignore` | `target/`, `.git/`, `.superpowers/`, `docs/`, `README_tmp.html` |
| `docker/config.toml` | `[server] host = "0.0.0.0"`, port 8787; providers: `qwen` (LM Studio via `http://192.168.1.10:8088/anthropic/v1/messages`, `api_key_env = "LMSTUDIO"`, `model = "qwen3.6:27b"`), `anthropic` (`auth_style = "anthropic"`), `deepseek`, `kimi`; default provider `qwen`; `[orchestrator]` block present but commented out (same posture as the repo config) |
| `docker-compose.yml` | Service `big-brother`: `build: .`, `ports: ["127.0.0.1:8787:8787"]`, `env_file: .env`, volume `./docker/config.toml:/app/config/config.toml:ro`, healthcheck `curl -fsS http://localhost:8787/health`, `restart: unless-stopped`. Service `open-webui`: `image: ghcr.io/open-webui/open-webui:main`, `ports: ["127.0.0.1:3000:8080"]`, named volume `open-webui-data:/app/backend/data`, env `OPENAI_API_BASE_URL=${LMSTUDIO_HOST}/v1`, `OPENAI_API_KEY=lm-studio`, `restart: unless-stopped` |
| `.env.example` | `DEEPSEEK_API_KEY=`, `KIMI_API_KEY=`, `Z_AI_API_KEY=`, `ANTHROPIC_API_KEY=`, `LMSTUDIO=lm-studio`, `LMSTUDIO_HOST=http://192.168.1.10:8088` |
| `.gitignore` | add `.env` |
| Docs | README: Docker quickstart in Usage + mention in How it works; USER_GUIDE: "Running with Docker" section (start/stop, config edits, where the panel and Open WebUI live, LM Studio reachability note) |

## Networking

- Containers → LM Studio: direct LAN IP via Docker's bridge NAT; no extra
  configuration. If LM Studio's IP differs from the example, the user edits
  `docker/config.toml` (proxy) and `.env` `LMSTUDIO_HOST` (Open WebUI).
- Host → proxy: `http://localhost:8787` (Claude Code, panel, curl).
- Host → Open WebUI: `http://localhost:3000`.
- Open WebUI → LM Studio directly (OpenAI dialect at `/v1`); it does not go
  through Big Brother (different API dialect).
- If a y-router is ever run on the host, container config should reference
  `http://host.docker.internal:8788/...` — documented, not shipped.

## Error handling

- Missing `.env` keys: existing proxy behavior (startup warning, 500 on use).
- Crash recovery: healthcheck + `restart: unless-stopped`.
- LM Studio down: surfaces through the orchestrator's existing `fail_mode`
  semantics and provider 502s, not Docker.

## Verification

1. `docker compose build` completes.
2. `docker compose up -d`; `docker compose ps` shows big-brother healthy.
3. From the host: `curl http://localhost:8787/health` → `{"status":"ok"}`;
   `curl -I http://localhost:8787/panel` → 200 text/html.
4. `http://localhost:3000` serves the Open WebUI login page.
5. `cargo test` still green natively (no source changes).

## Non-goals

- No y-router container, no LM Studio container, no TLS, no auth, no
  multi-arch publishing or registry pushes.
