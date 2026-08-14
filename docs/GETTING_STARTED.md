# Getting Started

A practical walkthrough for people who want to *use* Big Brother. No Rust
knowledge required. If you want the full configuration reference instead, read
the [User Guide](USER_GUIDE.md); for internals, see
[Architecture](ARCHITECTURE.md).

---

## What you get

Big Brother sits between you and the AI models you use, on your own machine.
Once it's running you get:

- **One address for every model.** Point Claude Code at Big Brother once, then
  switch between DeepSeek, Kimi, Z.ai, Anthropic, or a model running on your
  own hardware — without restarting anything.
- **A browser chat window.** Chat with the same models from a normal chat UI
  when you don't want a terminal.
- **Local-first answers with an escape hatch.** Optionally let a local model
  answer first and hand off to a cloud model only when it says the task is
  beyond it — with a cap on how many cloud calls per hour you'll pay for.
- **A control panel.** One page showing what's happening, with switches for
  the chat window.

Everything runs on your machine. Your prompts go to whichever provider you
pointed at, and nowhere else.

---

## Before you start

You need **one** of these:

| Route | You need | Best if |
|-------|----------|---------|
| **Docker** (recommended) | Docker Desktop, or Docker Engine with Compose v2.24+ | You want the chat window, dashboards, and metrics without assembling them |
| **Native** | Rust toolchain (`cargo`) | You only want the proxy, or you're developing on it |

Optional but useful: [LM Studio](https://lmstudio.ai/) on your machine or LAN
if you want a local model in the mix.

You'll also want at least one provider API key (DeepSeek, Kimi, Z.ai, or
Anthropic). You can start with only a local model and no keys at all.

---

## Route A — Docker (five minutes)

### 1. Get your keys in place

```sh
copy .env.example .env      # Windows
cp .env.example .env        # Linux / macOS
```

Open `.env` and fill in the keys you actually have. Leave the rest blank —
a blank key just means that provider is unavailable until you fill it in.

### 2. Point it at your local model (optional)

If you're using LM Studio, open `docker/config.toml` and set your machine's
LAN IP in **two** places:

```toml
[chat]
passthrough_url = "http://192.168.1.10:8088/v1/chat/completions"   # <- here

[providers.qwen]
base_url = "http://192.168.1.10:8088/anthropic/v1/messages"        # <- and here
```

In LM Studio, turn on **Serve on Local Network** (Developer tab). Skip this
step entirely if you're only using cloud providers — but then change
`[default] provider` to one you do have a key for.

### 3. Start everything

```sh
docker compose up -d --build
```

Four things come up, all bound to your machine only:

| What | Where | For |
|------|-------|-----|
| Big Brother | <http://localhost:8787> | The proxy; control panel at `/panel` |
| Open WebUI | <http://localhost:3000> | The chat window |
| docexport | <http://localhost:8789> | Turns responses into `.docx` / `.pdf` |
| Prometheus | <http://localhost:9090> | Raw metrics (you rarely need this) |
| Grafana | <http://localhost:3001> | Dashboards (`admin` / your `GRAFANA_ADMIN_PASSWORD`) |

### 4. Check it's alive

Open <http://localhost:8787/panel>. You should see the status page with your
providers listed and a green dot. Any provider showing **missing** just needs
its key in `.env` (then run `docker compose up -d` again — key changes need a
container recreate, not just a restart).

---

## Route B — Native

```sh
# 1. Set the keys you need
export DEEPSEEK_API_KEY=...          # set DEEPSEEK_API_KEY=... on Windows

# 2. Run it
cargo run --release
```

It reads `./config.toml` by default. To use a different file, pass it as an
argument (`cargo run --release -- my-config.toml`) or set `PROXY_CONFIG`. The
path it actually loaded is printed at startup.

The native route gives you the proxy and panel. The chat window, Prometheus,
and Grafana are part of the Docker stack.

---

## Using it: two front doors

### Claude Code

Point Claude Code at the proxy and run it as normal:

```sh
export ANTHROPIC_BASE_URL="http://localhost:8787"
export ANTHROPIC_API_KEY="dummy"      # required, but unused — real keys live in .env
claude
```

The `ANTHROPIC_API_KEY` value genuinely doesn't matter; Claude Code refuses to
start without one, and Big Brother substitutes the real provider key.

Switch models mid-session with Claude Code's own `/model` command:

```
/model deepseek/deepseek-chat     # a specific provider and model
/model deepseek                   # that provider's configured default model
```

### The chat window

Open <http://localhost:3000> and start typing. Open WebUI will offer exactly
one model, `big-brother` — that's deliberate. Which real model answers is
decided in the control panel, so the two can't disagree.

---

## The control panel

<http://localhost:8787/panel> refreshes every three seconds.

**Chat card** — the only interactive part:

- **pipeline** — when ON, chat messages go through the same routing and
  escalation machinery Claude Code uses. When OFF, they go straight to your
  local model, untouched.
- **target** — where chat goes when the pipeline is ON. Pick `cascade` to use
  the local-first behavior, or pin chat to one specific `provider/model`.

Changes take effect on the very next message — no restart — and they survive
restarts too.

**Everything else is read-only:** orchestrator settings, the hourly cloud
budget bar (amber at 80%, red when exhausted), how many conversations are
pinned to the cloud, and the last 500 escalations with timestamps and reasons.

No message content ever appears on this page. Conversations show up only as
8-character fingerprints, and API keys only as present/missing.

---

## Turning on local-first escalation

Out of the box the escalation feature is off — `cascade` simply means "use the
default provider." To get the real behavior (local model answers first, hands
off to a cloud model when it's out of its depth), uncomment the
`[orchestrator]` block in `docker/config.toml`:

```toml
[orchestrator]
local_provider = "qwen"
escalation_provider = "anthropic"
escalation_model = "claude-opus-5"
# max_cloud_requests_per_hour = 50    # your spend guardrail
```

Then `docker compose restart big-brother`.

Two things worth knowing:

- **Once a conversation escalates, it stays escalated.** Handing a thread back
  and forth mid-discussion produces worse answers than keeping it with the
  stronger model.
- **The hourly budget is shared** between Claude Code and the chat window.
  It's one bucket because it's one bill.

Full semantics — including what happens when the local model is unreachable —
are in the [User Guide](USER_GUIDE.md#hierarchical-orchestrator-local-first-with-cloud-escalation).

---

## Everyday tasks

| I want to… | Do this |
|------------|---------|
| Save a response as Word or PDF | Click **Export Response** under the message (one-time setup in [docexport](../docexport/README.md)) |
| Use a different model in Claude Code | `/model <provider>/<model>` in your session |
| Change which model the chat window uses | Panel → Chat card → **target** |
| Chat with my local model only, nothing fancy | Panel → Chat card → turn **pipeline** off |
| Stop paying for cloud calls right now | Panel → Chat card → target `cascade` off, or lower `max_cloud_requests_per_hour` and restart |
| Add a provider key I forgot | Edit `.env`, then `docker compose up -d` |
| Change routing defaults or add a provider | Edit `docker/config.toml`, then `docker compose restart big-brother` |
| See what's actually happening | `docker compose logs -f big-brother` |
| Stop everything | `docker compose down` (add `-v` to wipe chat history and dashboards too) |

---

## When something goes wrong

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| Panel says "proxy unreachable" | Container isn't running | `docker compose ps`, then `docker compose logs big-brother` |
| A provider shows **missing** | Key absent from `.env` | Add it, then `docker compose up -d` (recreate, not restart) |
| Chat window replies with an error about `[chat]` | No `[chat]` section in the config the container loaded | Add one to `docker/config.toml` (see the example above) and restart |
| Chat window shows no models | Open WebUI can't reach the proxy | Confirm both are up; the compose file wires them on a shared network |
| Everything times out against a local model | LM Studio not serving on the network, or wrong IP | Enable **Serve on Local Network**; check the IP in *both* config spots |
| Cloud model never gets used | Orchestrator still commented out, or budget exhausted | Uncomment `[orchestrator]`; check the budget bar on the panel |
| Claude Code refuses to start | `ANTHROPIC_API_KEY` unset | Set it to any non-empty string |

Still stuck? `docker compose logs -f big-brother` shows every routing
decision, and the [User Guide's troubleshooting
section](USER_GUIDE.md#troubleshooting) covers less common cases.

---

## Where to go next

- [User Guide](USER_GUIDE.md) — every config option, model-switching rules,
  the orchestrator in depth
- [Architecture](ARCHITECTURE.md) — diagrams of how requests actually flow
- [Developer Guide](DEVELOPER_GUIDE.md) — building, testing, and extending it
- [Product Brief](PRODUCT_BRIEF.md) — the short version of what this is and
  who it's for
