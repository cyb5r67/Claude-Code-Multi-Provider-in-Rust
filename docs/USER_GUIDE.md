# User Guide

`claude-multi-proxy` is a local reverse proxy that lets [Claude Code](https://claude.com/claude-code)
talk to any Anthropic-Messages-API-compatible backend — cloud providers
(DeepSeek, Kimi, Z.AI, OpenRouter via y-router) or local servers such as
LM Studio — and switch between them mid-session with the `/model` command.

- [Quick start](#quick-start)
- [The config file](#the-config-file)
- [Switching providers and models](#switching-providers-and-models)
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
INFO claude_multi_proxy: loaded config path=config.toml
INFO claude_multi_proxy: API key present provider=deepseek env=DEEPSEEK_API_KEY
INFO claude_multi_proxy: proxy listening on http://127.0.0.1:8787
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
INFO claude_multi_proxy::proxy: routing request provider=qwen model=qwen3.6:27b base_url=http://192.168.1.10:8088/...
```

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
| Claude Code warns: *Auth conflict: Both a token (ANTHROPIC_AUTH_TOKEN) and an API key (ANTHROPIC_API_KEY) are set* | Both env vars are set in Claude Code's shell | Unset the one you don't use; with this proxy you only need `ANTHROPIC_API_KEY=dummy` |
| Requests to `openrouter` loop forever / stack overflow | `base_url` points at the proxy's own port | Run y-router on its own port (e.g. 8788) and point `base_url` there |
