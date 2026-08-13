# Big Brother — Product Brief

**For:** marketing, sales, partnerships, and anyone writing or reviewing
external copy about this project.

**Rule of thumb for this document:** every claim below is either verifiable
from the source code or explicitly flagged as unverified. The
[Claims discipline](#claims-discipline) section at the end lists things we
must *not* say until someone measures them. Please keep it that way.

---

## At a glance

| | |
|---|---|
| **What it is** | A self-hosted router that sits between your AI coding tools and the LLM providers behind them |
| **Category** | Developer infrastructure / LLM gateway |
| **Deployment** | Runs on your own machine or server; single Rust binary, or a four-service Docker stack |
| **License** | Apache 2.0 — free to use, modify, and ship commercially |
| **Owner** | Cyb5r LLC |
| **Status** | Working software with automated test coverage; pre-1.0 |
| **Heritage** | Rust reimplementation of an open-source Python proxy (`simple-proxy.py` by spideynolove), with substantial additions |

---

## The one-liner

> **Big Brother routes your AI coding assistant to whichever model makes sense
> for the task — a free local one for the easy stuff, a frontier model when it
> actually matters — without changing how you work.**

---

## The problem

Developers using AI coding assistants face three frictions at once:

1. **Lock-in to one provider.** Most tools point at a single vendor's endpoint.
   Trying a cheaper or better model means reconfiguring, restarting, or
   switching tools entirely.
2. **Paying frontier prices for trivial work.** The same premium model that
   untangles a race condition also gets asked to rename a variable. Every
   request costs the same regardless of how hard it was.
3. **No visibility.** When spend climbs, there's rarely an answer to "which
   requests actually needed the expensive model?"

The workaround most teams reach for — manually switching models — requires
knowing in advance how hard a task is. Nobody does.

---

## What Big Brother does

It's a small program that runs on your own machine. Your AI tool talks to it
instead of talking to a provider directly. From there it can:

**Route to any provider, instantly.** One address serves every model you've
configured. Switch mid-conversation with a single command; nothing restarts.

**Let the cheap model try first.** Optionally, a local model on your own
hardware answers first. If it judges a task beyond its ability, it says so and
the request is handed to a frontier model automatically — invisibly, mid-flight,
with no lost context. The user just gets a good answer.

**Cap what you spend.** A configurable hourly ceiling on cloud calls. Once it's
reached, work continues locally instead of quietly running up a bill.

**Show its work.** A live panel and Grafana dashboards report every routing
decision, every escalation and why it happened, and how much of the hourly
budget is left.

**Serve a chat window too.** The same routing works from a normal browser chat
UI, not just the command line — controlled from the same panel.

---

## Capabilities and why they matter

| Capability | What it means for the user |
|------------|---------------------------|
| Multi-provider routing | Anthropic, DeepSeek, Moonshot/Kimi, Z.ai, OpenRouter, and any local or self-hosted endpoint speaking a supported API |
| In-session model switching | Change models with one command; no restart, no lost conversation |
| Local-first escalation | The expensive model is engaged only when the local one signals it's needed |
| Sticky escalation | Once a conversation is escalated it stays with the stronger model — no jarring quality swings mid-thread |
| Hourly spend cap | A hard, configurable ceiling on cloud calls; work degrades gracefully instead of failing or overspending |
| Two front doors | Works with Claude Code (Anthropic API) *and* OpenAI-compatible chat clients from the same install |
| Runtime controls | Chat routing changes from a web panel take effect on the next message and survive restarts |
| Full observability | Prometheus metrics and a provisioned Grafana dashboard included |
| Streaming preserved | Responses stream token-by-token, unbuffered — no added latency from the proxy re-assembling them |
| Self-hosted | Runs on your hardware; bound to localhost by default; no Big Brother cloud service, no telemetry, no account |

---

## Who it's for

**The cost-conscious individual developer.** Runs a capable open model on a
workstation or gaming PC and pays for frontier tokens only when the local model
taps out. Wants the savings without babysitting a model picker.

**The small engineering team.** Wants provider optionality — the freedom to
adopt a better or cheaper model next quarter without re-tooling — plus a shared
view of where the AI budget actually goes.

**The privacy- or compliance-sensitive shop.** Needs routine work to stay on
hardware they control, with deliberate, auditable exceptions when a request
goes to a third party. Every escalation is logged with a timestamp and reason.

---

## What makes it different

- **The escalation decision is made by the model doing the work**, not by a
  classifier guessing difficulty up front, and not by the user picking a model
  before they know how hard the task is.
- **Escalation is invisible to the client.** No plugin, no special client, no
  changed workflow — the calling tool doesn't know or care that a handoff
  happened.
- **Spend control is a first-class feature**, not an afterthought: a real
  sliding-window budget with a documented, graceful degradation path.
- **One install, two protocols.** The terminal tool and the browser chat window
  share routing, budget, and audit trail.
- **Self-hosted with no vendor in the middle.** There is no hosted service to
  sign up for, and no path by which prompts reach us — we never see them.

**Honest comparison note:** other LLM gateways and routers exist, several with
larger provider catalogs and more mature ecosystems. Big Brother's distinctive
combination is *self-hosted + local-first automatic escalation + hard budget
ceiling + built-in observability*. Lead with that combination, not with claims
of general superiority.

---

## Proof points (all verifiable in the repository)

- Written in **Rust** — a single binary, no runtime to install.
- **134 automated tests** covering routing, dialect translation, streaming,
  escalation logic, budget enforcement, and settings persistence.
- **Apache 2.0 licensed**, with attribution to the original Python project.
- **Four-service Docker stack** — proxy, chat UI, Prometheus, Grafana — that
  comes up with one command.
- **Nine Prometheus metric families** exposed out of the box.
- **Privacy by construction:** the status panel and audit log contain no
  message content — conversations appear only as 8-character hashes, and API
  keys only as present/missing.
- **Localhost-bound by default**; all published Docker ports bind to
  `127.0.0.1`.

---

## What it is *not*

Being straight about scope prevents the worst kind of customer disappointment.

- **Not a model host.** It routes to models; it doesn't run them. You supply
  local models (e.g. via LM Studio) or provider API keys.
- **Not a hosted service.** There's nothing to sign up for. It's software you
  run.
- **Not a multi-agent framework.** It doesn't orchestrate autonomous agents,
  and it isn't an A2A/MCP participant. It routes individual requests.
- **Not a drop-in for every client yet.** The chat endpoint covers text
  conversation; tool calling and image input are not supported on that path.
- **Not a security boundary.** The control panel has no authentication and is
  meant to run on a trusted machine, bound to localhost.
- **Not benchmarked.** We have no published latency or cost-savings figures.
  See below.

---

## Messaging bank

**One sentence (30 characters or fewer, for a card or tab):**
> Route AI work to the right model.

**One sentence (standard):**
> Big Brother is a self-hosted router that sends your AI coding assistant's
> easy work to a local model and the hard parts to a frontier model —
> automatically.

**Elevator pitch (~30 seconds):**
> If you use an AI coding assistant, you're probably paying premium prices for
> every request — including the trivial ones. Big Brother runs on your machine
> between your tool and the model providers. It lets a local model try first;
> when that model says a task is beyond it, the request is handed to a frontier
> model automatically, mid-flight, with no interruption. You set a ceiling on
> how many cloud calls per hour you'll allow, and a dashboard shows you exactly
> where the money went. It's open source, self-hosted, and works with the tools
> you already use.

**Boilerplate paragraph (for press or a footer):**
> Big Brother is an open-source, self-hosted LLM router from Cyb5r LLC. It
> gives developers one endpoint for every model they use, automatic
> local-first escalation to frontier models when a task demands it, hard
> hourly limits on cloud spend, and built-in observability. Written in Rust
> and licensed under Apache 2.0.

**Tagline candidates:**
- *The right model for the task. Automatically.*
- *Local first. Frontier when it counts.*
- *Your models. Your machine. Your budget.*

**Words to prefer:** routes, escalates, hands off, budget ceiling,
self-hosted, observable, provider-neutral.

**Words to avoid:** *monitors*, *watches*, *surveils* — the project name
already invites an Orwellian reading, and this product's actual privacy
posture is the opposite (nothing leaves your machine, no content is logged).
Lean into "you control it," never "it watches you." Also avoid *agentic* and
*AI agent*; the product is deliberately not that.

---

## FAQ

**Does my code or prompts get sent to Cyb5r?**
No. There is no hosted component. Requests go from your machine to whichever
provider you configured, and nowhere else.

**Do I need a local model?**
No. Big Brother is useful purely as a multi-provider router. Local-first
escalation is optional and off by default.

**Will this work with my AI tool?**
It works with Claude Code and with chat clients that speak the OpenAI API.
Anything else needs checking case by case.

**Is it free?**
The software is free and Apache 2.0 licensed. You still pay your model
providers directly for whatever you use.

**How much will it save me?**
That depends entirely on your model mix and workload, and we have not published
measurements. See the next section.

**Is it production-ready?**
It is pre-1.0 software with solid automated test coverage. It's well suited to
individual developers and small teams; treat wider deployment as an evaluation.

---

## Claims discipline

These are the claims most likely to get made by accident. **Don't ship any of
them until someone measures and documents the result.**

| Don't say | Why not | Say instead |
|-----------|---------|-------------|
| "Cuts AI costs by 70%" (or any figure) | No measurement exists | "Designed to reduce cloud spend by answering routine work locally" |
| "Faster than calling providers directly" | The proxy adds a hop; streaming is unbuffered, but no latency benchmark exists | "Streams responses through unbuffered, so the proxy doesn't add buffering delay" |
| "Enterprise-ready" / "SOC 2" / "hardened" | No audit, no auth on the panel, pre-1.0 | "Self-hosted and open source; evaluate for your environment" |
| "Works with any AI tool" | Two protocols supported, with documented gaps | "Works with Claude Code and OpenAI-compatible chat clients" |
| "The local model handles most requests" | Depends entirely on workload; unmeasured | "The local model handles what it can and escalates what it can't" |
| "Secure by default" | The panel is unauthenticated by design | "Runs entirely on hardware you control, bound to localhost by default" |
| "Supports MCP / agents / A2A" | It doesn't, deliberately | Omit |

If you need a hard number for a campaign, ask engineering to run and publish a
measurement first. A verified modest number is worth more than an impressive
one we'd have to retract.

---

## Assets and further reading

- [Getting Started](GETTING_STARTED.md) — the five-minute setup, good source
  material for a demo script or screencast
- [User Guide](USER_GUIDE.md) — complete feature behavior
- [Architecture](ARCHITECTURE.md) — diagrams suitable for technical slides
- [Developer Guide](DEVELOPER_GUIDE.md) — for engineering-audience content
