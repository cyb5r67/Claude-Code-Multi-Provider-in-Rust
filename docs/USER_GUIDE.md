# User Guide

Big Brother is a local reverse proxy that lets [Claude Code](https://claude.com/claude-code)
talk to any Anthropic-Messages-API-compatible backend — cloud providers
(DeepSeek, Kimi, Z.AI, OpenRouter via y-router) or local servers such as
LM Studio — and switch between them mid-session with the `/model` command.

- [Quick start](#quick-start)
- [The config file](#the-config-file)
- [Switching providers and models](#switching-providers-and-models)
- [Status panel](#status-panel)
- [Running with Docker](#running-with-docker)
- [Example: local LM Studio hosts](#example-local-lm-studio-hosts)
- [Logging](#logging)
- [Troubleshooting](#troubleshooting)

---

## Quick start

```sh
# 1. Set the API-key env vars your config references (see below).
export DEEPSEEK_API_KEY=...

# 2. Start the proxy. The config path is the first argument.
cargo run --release -- config.toml

# 3. In another shell, point Claude Code at the proxy.
export ANTHROPIC_BASE_URL="http://localhost:8787"
export ANTHROPIC_API_KEY="dummy"
claude
```

The first log line confirms which config file was loaded:

```
INFO big_brother: loaded config path=config.toml
INFO big_brother: API key present provider=deepseek env=DEEPSEEK_API_KEY
INFO big_brother: proxy listening on http://127.0.0.1:8787
```

**Always check the `loaded config path=` line.** The path is resolved in this
order — first CLI argument, then the `PROXY_CONFIG` env var, then `./config.toml`
— so if you expect a custom file, make sure that's the one it actually loaded.

---

## The config file

```toml
[server]                       # optional; these are the defaults
host = "127.0.0.1"
port = 8787
request_timeout_secs = 300

[default]                      # required: used when nothing selects a provider
provider = "deepseek"
model = "deepseek-chat"

[providers.deepseek]           # one section per provider; the name is yours
base_url = "https://api.deepseek.com/anthropic/v1/messages"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"        # optional: default model for `/model deepseek`
```

| Key | Required | Meaning |
|-----|----------|---------|
| `providers.<name>` | yes (≥1) | Provider name — this is what you type in `/model <name>/...` |
| `base_url` | yes | Full URL of the provider's Anthropic-compatible `/v1/messages` endpoint |
| `api_key_env` | yes | Name of the **environment variable** holding the key (keys never live in the file) |
| `model` | no | Default model used when the provider is selected by bare name |

Two rules that trip people up:

- **The key env var must be set in the shell that runs the proxy**, not the one
  running Claude Code. It must be non-empty; for servers that don't check keys
  (LM Studio), any value works: `export LMSTUDIO=lm-studio`.
- **Keys are resolved per request**, but presence is checked at startup — a
  `WARN ... API key NOT set` line means requests to that provider will fail
  with a 500 until you export the variable and restart.

---

## Switching providers and models

Claude Code's `/model` command sends whatever you type as the request's model
id. The proxy interprets it like this:

| You type | Provider used | Model sent upstream |
|----------|---------------|---------------------|
| `/model deepseek/deepseek-chat` | `deepseek` | `deepseek-chat` |
| `/model deepseek` | `deepseek` | its configured `model` (bare names need one) |
| `/model openai/openai/gpt-oss-20b` | `openai` | `openai/gpt-oss-20b` — only the **first** `/` splits provider from model |
| `/model x-ai/grok-code-fast-1` | *default provider* | `x-ai/grok-code-fast-1` unchanged, because `x-ai` is not a configured provider |

So: if the upstream model id itself contains a `/` (common for OpenRouter and
LM Studio ids like `openai/gpt-oss-20b`), either prefix it with the provider
name (`/model openai/openai/gpt-oss-20b`) or set it as the provider's `model`
in the config and select by bare name (`/model openai`).

Every routed request is logged, so you can verify a switch landed where you
expected:

```
INFO big_brother::proxy: routing request provider=qwen model=qwen3.6:27b base_url=http://192.168.1.10:8088/...
```

---

## Hierarchical orchestrator (local-first with cloud escalation)

With an `[orchestrator]` section in the config, Big Brother answers every
fresh conversation with the **local tier** first (e.g. Qwen on LM Studio). The
proxy injects a system instruction telling the local model to output the
sentinel token (`<<ESCALATE>>` by default) as its very first token when a task
is beyond it. When that happens, the proxy silently replays the original
request to the **escalation provider** and the conversation stays on the cloud
tier until it ends ("sticky").

```toml
[orchestrator]
local_provider = "qwen"
escalation_provider = "anthropic"
escalation_model = "claude-opus-5"

[providers.qwen]
base_url = "http://192.168.1.10:8088/anthropic/v1/messages"
api_key_env = "LMSTUDIO"
model = "qwen3.6:27b"

[providers.anthropic]
base_url = "https://api.anthropic.com/v1/messages"
api_key_env = "ANTHROPIC_API_KEY"
auth_style = "anthropic"
```

Rules of thumb:

- **You always win:** `/model provider/model` bypasses orchestration for that
  request.
- **Budget guard:** at most `max_cloud_requests_per_hour` escalations per hour
  (default 50). Beyond that, requests are answered locally and a warning is
  logged.
- **Audit:** every escalation logs one line —
  `escalating to cloud tier trigger=sentinel provider=anthropic model=claude-opus-5`.
- **Local tier down?** `fail_mode = "cloud"` (default) escalates;
  `fail_mode = "error"` surfaces the error as before.
- Local attempts are sent with the local provider's configured `model`;
  escalations are sent with `escalation_model`.

### Limitations

- **Pass-through model ids are orchestrated.** Only a model id naming a
  configured provider (`/model anthropic/claude-opus-5`) bypasses
  orchestration. A slash id whose prefix is *not* a configured provider
  (e.g. `/model x-ai/grok-code-fast-1`) is treated as default-routed and
  will be answered by the orchestrator's tiers — the id itself is replaced
  by the tier's model. To force a specific upstream, name a configured
  provider.
- **No dedicated first-token timeout.** A hung (but connected) local server
  is only rescued by the global `request_timeout_secs` (default 300s),
  after which `fail_mode` applies. Lower `request_timeout_secs` if a wedged
  local tier should fail over faster.

---

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

## Chat window (Open WebUI)

With a `[chat]` section in the config, the proxy exposes an OpenAI-dialect
front door (`POST /v1/chat/completions`, `GET /v1/models`) so a browser chat
client can use the same routing machinery as Claude Code. In the Docker
stack, Open WebUI (<http://localhost:3000>) is pre-wired to it.

```toml
[chat]
passthrough_url = "http://192.168.1.10:8088/v1/chat/completions"
passthrough_model = "qwen3.6:27b"
# pipeline_enabled = true      # default
# model_override = "cascade"   # default
# state_file = "chat_state.json"
```

Control it from the panel's **Chat** card (no restart needed):

- **pipeline** toggle — ON sends chat through routing/cascade/escalation
  (sharing the hourly cloud budget with Claude Code traffic); OFF forwards
  requests straight to `passthrough_url` with only the model id rewritten
  to `passthrough_model`.
- **target** dropdown — `cascade` (local-first with escalation, or the
  default provider when no orchestrator is configured) or a specific
  `provider/model` to pin chat to one upstream.

Panel edits persist to `state_file` and survive restarts; the `[chat]`
values are only first-run defaults. Open WebUI's own model picker shows a
single `big-brother` model by design — routing is decided here, not there.
Tool calling and images are not supported through the chat endpoint.

Note: `PUT /chat/settings` is the panel's first mutating endpoint. The
panel is unauthenticated, which stays acceptable only while the proxy is
reachable from localhost alone — don't widen the bind without adding auth.

---

## Exporting responses to .docx / .pdf

The stack includes `docexport`, a sidecar that converts a chat response into a
Word document or PDF, and an Open WebUI Action that adds an **Export
Response** button under each message.

```sh
docker compose up -d --build docexport
curl http://localhost:8789/health      # {"status":"ok"}
```

Then paste `docexport/openwebui_action.py` into Open WebUI under **Admin
Panel → Functions → New Function**, replacing the Filter scaffold the editor
pre-fills. Switch the function **Active**, then turn on **Global** in its
**…** menu so the button attaches to every model. Clicking it appends
download links to the chat; files expire after an hour.

Conversion runs through pandoc, so headings, nested lists, tables, fenced
code blocks, blockquotes and task lists all survive as real document
structure. PDFs render via WeasyPrint with a print stylesheet — A4, page
numbers, and fonts that cover accented characters, Greek letters and emoji.

Two behaviors are deliberate: images are downgraded to links rather than
fetched and embedded, and raw HTML is stripped. Both keep untrusted model
output from driving the renderer or making outbound requests.

Full reference, including the Valves you may need to change if you browse to
Open WebUI from another machine: [`docexport/README.md`](../docexport/README.md).

---

## Running with Docker

Requires Docker Desktop (or Docker Engine with Compose v2.24+; the compose
file uses the long-form optional `env_file` syntax).

```sh
copy .env.example .env        # Windows (cp on Linux/macOS); fill in your keys
docker compose up -d --build
```

What comes up:

| Service | Address | Purpose |
|---------|---------|---------|
| `big-brother` | <http://localhost:8787> | The proxy — Claude Code target, panel at `/panel` |
| `open-webui`  | <http://localhost:3000> | Chat UI, routed through the proxy (see [Chat window](#chat-window-open-webui)) |
| `docexport`   | <http://localhost:8789> | Converts responses to `.docx`/`.pdf` (see [below](#exporting-responses-to-docx--pdf)) |
| `prometheus`  | <http://localhost:9090> | Scrapes the proxy's `/metrics` every 15 s |
| `grafana`     | <http://localhost:3001> | Dashboard over Prometheus (`admin` / `GRAFANA_ADMIN_PASSWORD`) |

All ports are published to `127.0.0.1` by default, so nothing is reachable
from your network even though the proxy binds `0.0.0.0` inside its container.
To share the chat UI, see [Exposing the chat UI to your
LAN](#exposing-the-chat-ui-to-your-lan).

- **Config:** the container reads `docker/config.toml` (not the repo-root
  `config.toml`). Edit it, then `docker compose restart big-brother`.
- **Keys** come from `.env` (gitignored). After editing `.env`, run
  `docker compose up -d` again — env changes need a container recreate
  (`docker compose restart` applies only `docker/config.toml` edits).
- **LM Studio stays on your LAN machine** — containers reach it by IP.
  Set that IP in `docker/config.toml` in **two** places —
  `[providers.qwen] base_url` (Anthropic dialect, for the pipeline) and
  `[chat] passthrough_url` (OpenAI dialect, for passthrough mode) — and keep
  LM Studio's **Serve on Local Network** enabled.
- **Logs:** `docker compose logs -f big-brother` shows the same tracing
  output as a native run, escalation audit lines included.
- **Stop:** `docker compose down` (add `-v` to also wipe Open WebUI,
  Prometheus, and Grafana data).
- If you later run a y-router on the host, point the container's provider at
  `http://host.docker.internal:8788/v1/messages`.
- **Metrics:** the proxy exposes Prometheus text format at
  `http://localhost:8787/metrics` (requests by provider/outcome, latency
  histograms, tier dispatches, escalations by trigger, budget and sticky
  gauges). The bundled Grafana dashboard ("Big Brother") is provisioned
  from `docker/grafana/dashboards/big-brother.json` — edit the JSON and
  restart Grafana to change it. History lives in the `prometheus-data`
  volume (default retention).

---

## Exposing the chat UI to your LAN

Two services can safely leave localhost; three must not.

```ini
# .env
WEBUI_BIND_ADDR=0.0.0.0
DOCEXPORT_BIND_ADDR=0.0.0.0
```

Then `docker compose up -d`. Open WebUI becomes reachable at
`http://<host-ip>:3000` and document exports at `<host-ip>:8789`.

**Export the sidecar too, or downloads break.** The export link is followed by
the browser, not the server, so a client that isn't the Docker host cannot
reach a localhost-bound sidecar. The Action derives the link from the address
you browsed to, so no reconfiguration is needed once the port is published.

**Never expose the proxy, Prometheus, or Grafana.** `big-brother` keeps
`127.0.0.1` deliberately: its panel has no authentication and `PUT
/chat/settings` lets any caller change routing and burn cloud budget. Open
WebUI reaches the proxy over the internal Docker network, so nothing is lost
by keeping it closed. Open WebUI is different — it has real accounts.

**Windows hosts need a firewall rule.** Docker publishing a port is not
enough; the Windows Firewall drops inbound connections by default, which looks
like a hang rather than a refusal. From an elevated PowerShell:

```powershell
New-NetFirewallRule -DisplayName "Big Brother chat (LAN)" -Direction Inbound `
  -Action Allow -Protocol TCP -LocalPort 3000,8789 -RemoteAddress LocalSubnet
```

`-RemoteAddress LocalSubnet` keeps the opening to your own network.

**Before you share the URL**, check three things: new sign-ups are disabled
(Admin Panel → Settings → General) unless you want anyone on the network
creating accounts; the connection is plain HTTP, so passwords cross the LAN in
clear text; and the exports sidecar has no login at all, so anyone who can
reach it can convert documents.

---

## Example: local LM Studio hosts

One provider per machine, each serving a different model. LM Studio doesn't
validate API keys, but the proxy requires a non-empty env var — all providers
can share one:

```toml
[default]
provider = "qwen"
model = "qwen3.6:27b"

[providers.qwen]
base_url = "http://192.168.1.10:8088/anthropic/v1/messages"
api_key_env = "LMSTUDIO"
model = "qwen3.6:27b"

[providers.openai]
base_url = "http://192.168.1.150:1234/anthropic/v1/messages"
api_key_env = "LMSTUDIO"
model = "openai/gpt-oss-20b"
```

```sh
export LMSTUDIO=lm-studio
cargo run --release -- myconfig.toml
```

Then `/model qwen` and `/model openai` switch between the machines.

On each LM Studio host: start the server (Developer tab), enable **Serve on
Local Network** (otherwise it only listens on `127.0.0.1` and other machines
get connection timeouts), and confirm the port matches your `base_url`.

---

## Logging

Set `RUST_LOG` before starting (default `info`):

```sh
RUST_LOG=debug cargo run --release -- config.toml
```

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `API error: 500 ... API key environment variable 'X' not set for provider 'Y'` | The env var named by that provider's `api_key_env` is unset or empty in the **proxy's** shell | `export X=...` (any non-empty value for LM Studio) and restart the proxy |
| Startup WARNs mention providers you didn't configure | The proxy loaded a different config file than you intended | Check the `loaded config path=` line; pass the path explicitly: `cargo run -- myconfig.toml` |
| Errors always name the same provider no matter what `/model` you pick | You're running a pre-July-2026 build that ignored the model field | `git pull && cargo build` |
| `502 ... failed to reach provider 'X': error sending request` | Transport failure: the proxy couldn't connect to `base_url`. A ~30 s delay before the error means a connect timeout (host down, wrong IP/port, or a firewall drop); an instant error means connection refused (nothing listening on that port) | From the proxy machine: `curl http://<host>:<port>/v1/models`. Verify the server is running, the port matches the config, and (LM Studio) **Serve on Local Network** is enabled |
| Upstream returns "model not found" | The model id sent upstream isn't what the server expects — remember only the text after the first `/` is forwarded | Use the full-id form (`/model openai/openai/gpt-oss-20b`) or set the provider's `model` in config and select by bare name |
| Claude Code: `Unable to validate model: undefined is not an object (evaluating 'R.usage.input_tokens')` | `/model` validation sends a test request and reads `usage.input_tokens` from the response; the backend answered (often 200) with a body that is not a complete Anthropic Messages response — common with local-server compatibility shims | Inspect what the backend really returns: `curl -s http://<host>:<port>/<path> -H 'content-type: application/json' -d '{"model":"<id>","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'` — if `usage.input_tokens` is missing, the backend's Anthropic compatibility is incomplete; check its version/settings or front it with a translator such as y-router |
| Claude Code warns: *Auth conflict: Both a token (ANTHROPIC_AUTH_TOKEN) and an API key (ANTHROPIC_API_KEY) are set* | Both env vars are set in Claude Code's shell | Unset the one you don't use; with this proxy you only need `ANTHROPIC_API_KEY=dummy` |
| Requests to `openrouter` loop forever / stack overflow | `base_url` points at the proxy's own port | Run y-router on its own port (e.g. 8788) and point `base_url` there |
