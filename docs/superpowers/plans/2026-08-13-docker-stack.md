# Docker Stack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run Big Brother + Open WebUI under Docker Compose with one command, publishing both services to host-localhost only, with LM Studio reached over the LAN and Claude Code connecting from the host unchanged.

**Architecture:** Packaging only — zero Rust changes. A multi-stage Dockerfile builds the release binary into a slim Debian runtime; a container-specific `docker/config.toml` (binding `0.0.0.0`) is volume-mounted read-only; compose wires secrets from a gitignored `.env` and adds the official Open WebUI image pointed at LM Studio.

**Tech Stack:** Docker / Docker Compose v2 (Docker Desktop on Windows), `rust:1-slim` + `debian:bookworm-slim` images, `ghcr.io/open-webui/open-webui:main`.

**Spec:** `docs/superpowers/specs/2026-08-13-docker-stack-design.md`

## Global Constraints

- **No Rust source changes.** `src/`, `tests/`, `Cargo.*` are untouched.
- Proxy binds `0.0.0.0` **inside** the container; compose publishes **`127.0.0.1:8787:8787`** and **`127.0.0.1:3000:8080`** — localhost-only on the host (the panel has no auth).
- Secrets only via `.env` (gitignored); `.env.example` documents every variable; no key ever lands in a tracked file.
- `docker/config.toml` is mounted read-only at `/app/config/config.toml`; edits apply via `docker compose restart big-brother`.
- Runtime image needs `ca-certificates` (rustls HTTPS) and `curl` (healthcheck).
- `cargo test` must remain green (nothing to change, but verify at the end).
- Windows host: prefer `docker compose` (v2, no hyphen); shell steps are Git Bash compatible.

---

### Task 1: Dockerfile and .dockerignore

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`

**Interfaces:**
- Produces: image with the binary at `/usr/local/bin/big-brother`, `CMD ["big-brother", "/app/config/config.toml"]`, non-root user `app`, port 8787 exposed. Task 2's compose file relies on exactly that CMD path and port.

- [ ] **Step 1: Create `.dockerignore`:**

```
target/
.git/
.superpowers/
docs/
README_tmp.html
.env
```

- [ ] **Step 2: Create `Dockerfile`:**

```dockerfile
# Stage 1: build the release binary.
FROM rust:1-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# Stage 2: minimal runtime.
FROM debian:bookworm-slim
# ca-certificates: rustls needs the system CA store for HTTPS to providers.
# curl: used by the compose healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --create-home app
COPY --from=builder /build/target/release/big-brother /usr/local/bin/big-brother
USER app
EXPOSE 8787
CMD ["big-brother", "/app/config/config.toml"]
```

(`src/panel.html` rides along with `COPY src` and is embedded at compile time via `include_str!`; `tests/` is not needed for a release build.)

- [ ] **Step 3: Verify the image builds**

Run: `docker build -t big-brother:dev .` (allow up to 10 minutes on a cold cache; use a 600000 ms tool timeout or run it in the background and wait)
Expected: `naming to docker.io/library/big-brother:dev` and exit 0.

- [ ] **Step 4: Verify the container refuses politely without a config** (proves CMD wiring):

Run: `docker run --rm big-brother:dev`
Expected: exits non-zero quickly with a log line like `failed to read config file '/app/config/config.toml'` — that is the correct behavior (config arrives via volume in Task 2).

- [ ] **Step 5: Commit**

```bash
git add Dockerfile .dockerignore
git commit -m "Add multi-stage Dockerfile for the proxy"
```

---

### Task 2: Compose file, container config, env template

**Files:**
- Create: `docker/config.toml`
- Create: `docker-compose.yml`
- Create: `.env.example`
- Modify: `.gitignore` (append `.env`)

**Interfaces:**
- Consumes: the image contract from Task 1 (CMD path `/app/config/config.toml`, port 8787).
- Produces: the full stack definition Task 3 smoke-tests.

- [ ] **Step 1: Create `docker/config.toml`:**

```toml
# Big Brother configuration for the Docker container.
#
# Mounted read-only at /app/config/config.toml. Edit this file on the host,
# then `docker compose restart big-brother` to apply. API keys come from
# .env via docker-compose's env_file -- never from this file.

[server]
# 0.0.0.0 is required inside a container: Docker's port mapping cannot reach
# a 127.0.0.1 bind. Exposure on the host is still localhost-only because
# docker-compose publishes the port as 127.0.0.1:8787:8787.
host = "0.0.0.0"
port = 8787
request_timeout_secs = 300

[default]
provider = "qwen"
model = "qwen3.6:27b"

[providers.qwen]
# LM Studio on the LAN host (Developer tab -> Serve on Local Network).
# Adjust the IP/port to your machine.
base_url = "http://192.168.1.10:8088/anthropic/v1/messages"
api_key_env = "LMSTUDIO"
model = "qwen3.6:27b"

[providers.anthropic]
base_url = "https://api.anthropic.com/v1/messages"
api_key_env = "ANTHROPIC_API_KEY"
auth_style = "anthropic"
model = "claude-opus-5"

[providers.deepseek]
base_url = "https://api.deepseek.com/anthropic/v1/messages"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"

[providers.kimi]
base_url = "https://api.moonshot.ai/anthropic/v1/messages"
api_key_env = "KIMI_API_KEY"

[providers.zai]
base_url = "https://api.z.ai/api/anthropic/v1/messages"
api_key_env = "Z_AI_API_KEY"

# --- Hierarchical orchestrator (optional) -----------------------------------
# Uncomment to answer conversations with Qwen first and transparently
# escalate to Claude when Qwen signals a task is beyond it.
# See docs/USER_GUIDE.md for semantics.
#
# [orchestrator]
# local_provider = "qwen"
# escalation_provider = "anthropic"
# escalation_model = "claude-opus-5"
# # sentinel = "<<ESCALATE>>"             # default
# # max_cloud_requests_per_hour = 50      # default; budget guard
# # fail_mode = "cloud"                   # "cloud" (default) or "error"
```

- [ ] **Step 2: Create `docker-compose.yml`:**

```yaml
services:
  big-brother:
    build: .
    ports:
      - "127.0.0.1:8787:8787" # localhost-only on the host (the panel has no auth)
    env_file:
      - path: ./.env
        required: false
    volumes:
      - ./docker/config.toml:/app/config/config.toml:ro
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://localhost:8787/health"]
      interval: 15s
      timeout: 3s
      retries: 3
      start_period: 5s
    restart: unless-stopped

  open-webui:
    image: ghcr.io/open-webui/open-webui:main
    ports:
      - "127.0.0.1:3000:8080"
    environment:
      # Open WebUI speaks the OpenAI dialect straight to LM Studio -- it does
      # not go through Big Brother (which speaks the Anthropic dialect).
      OPENAI_API_BASE_URL: "${LMSTUDIO_HOST:-http://192.168.1.10:8088}/v1"
      OPENAI_API_KEY: "lm-studio"
    volumes:
      - open-webui-data:/app/backend/data
    restart: unless-stopped

volumes:
  open-webui-data:
```

- [ ] **Step 3: Create `.env.example`:**

```
# Copy to .env and fill in what you use (.env is gitignored):
#   Windows:      copy .env.example .env
#   Linux/macOS:  cp .env.example .env

# LM Studio host, no trailing slash. Used by Open WebUI; the proxy's qwen
# provider URL lives in docker/config.toml instead.
LMSTUDIO_HOST=http://192.168.1.10:8088

# LM Studio ignores API keys, but the proxy requires a non-empty value.
LMSTUDIO=lm-studio

# Cloud provider keys. Leave blank to leave that provider unusable.
ANTHROPIC_API_KEY=
DEEPSEEK_API_KEY=
KIMI_API_KEY=
Z_AI_API_KEY=
```

- [ ] **Step 4: Append `.env` to `.gitignore`** (own line at the end of the file).

- [ ] **Step 5: Verify**

Run: `docker compose config --quiet`
Expected: exit 0, no output (compose file and env interpolations parse).

Run: `cargo test --lib config`
Expected: green — the schema is unchanged, so `docker/config.toml`'s keys all
correspond to fields these tests cover. End-to-end runtime verification of
the mounted config happens in Task 3's smoke test; no runtime check here.

- [ ] **Step 6: Commit**

```bash
git add docker/config.toml docker-compose.yml .env.example .gitignore
git commit -m "Add docker-compose stack with Open WebUI and container config"
```

---

### Task 3: Docs and live smoke test

**Files:**
- Modify: `README.md`
- Modify: `docs/USER_GUIDE.md`

- [ ] **Step 1: README** — in the "Usage" section, after the existing 3-step block, add:

```markdown
### Or with Docker

```sh
copy .env.example .env   # then fill in your keys (cp on Linux/macOS)
docker compose up -d --build
```

The proxy listens on <http://localhost:8787> (status panel at `/panel`) and
[Open WebUI](https://github.com/open-webui/open-webui) on
<http://localhost:3000> for chatting with your LM Studio models directly.
Edit `docker/config.toml` and `docker compose restart big-brother` to apply
config changes. Claude Code connects exactly as above.
```

- [ ] **Step 2: USER_GUIDE** — add a TOC entry `- [Running with Docker](#running-with-docker)` after the "Status panel" entry, and insert this section between "Status panel" and "Example: local LM Studio hosts":

```markdown
## Running with Docker

Requires Docker Desktop (or any Docker Engine with Compose v2).

```sh
copy .env.example .env        # Windows (cp on Linux/macOS); fill in your keys
docker compose up -d --build
```

What comes up:

| Service | Address | Purpose |
|---------|---------|---------|
| `big-brother` | <http://localhost:8787> | The proxy — Claude Code target, panel at `/panel` |
| `open-webui`  | <http://localhost:3000> | Chat UI talking straight to LM Studio (OpenAI dialect) |

Both ports are published to `127.0.0.1` only, so nothing is reachable from
your network even though the proxy binds `0.0.0.0` inside its container.

- **Config:** the container reads `docker/config.toml` (not the repo-root
  `config.toml`). Edit it, then `docker compose restart big-brother`.
- **Keys** come from `.env` (gitignored). `docker compose up` picks up edits
  after a `docker compose up -d` again.
- **LM Studio stays on your LAN machine** — containers reach it by IP.
  Update the IP in both `docker/config.toml` (proxy) and `.env`'s
  `LMSTUDIO_HOST` (Open WebUI), and keep LM Studio's **Serve on Local
  Network** enabled.
- **Logs:** `docker compose logs -f big-brother` shows the same tracing
  output as a native run, escalation audit lines included.
- **Stop:** `docker compose down` (add `-v` to also wipe Open WebUI's data).
- If you later run a y-router on the host, point the container's provider at
  `http://host.docker.internal:8788/v1/messages`.
```

- [ ] **Step 3: Live smoke test** (the host's port 8787 must be free — stop any natively running proxy first):

```bash
cp .env.example .env
docker compose up -d --build
docker compose ps
```

Expected: both services `Up`, big-brother eventually `healthy`.

```bash
curl -s http://localhost:8787/health
curl -s -o /dev/null -w "%{http_code} %{content_type}\n" http://localhost:8787/panel
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:3000
```

Expected: `{"status":"ok"}`; `200 text/html; charset=utf-8`; `200` (Open WebUI may take ~30 s to boot; retry until up).

Then run `cargo test` natively — expected green (97 tests), proving no source drift.

Leave the stack running for the user unless it failed.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/USER_GUIDE.md
git commit -m "Document the Docker stack"
```
