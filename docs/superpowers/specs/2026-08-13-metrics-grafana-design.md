# Design: Metrics + Grafana

Date: 2026-08-13
Status: Approved (user approved design and delegated execution end to end).

## Goal

Prometheus-format metrics from the proxy (`GET /metrics`) and a provisioned
Grafana dashboard in the Docker stack, so routing, escalations, budget, and
latency are visible as time series with zero manual Grafana setup.

## Decisions taken

| Question | Decision |
|---|---|
| Instrumentation | `prometheus` crate (text encoder only, `default-features = false`) — the one new dependency |
| First sub-project | This one; LLM start/stop control, config management (Phase B), and user management are separate future specs |
| Grafana port | `127.0.0.1:3001` (Open WebUI owns 3000) |
| Provisioning | Datasource + dashboard JSON committed in-repo; `docker compose up -d` lands on a working dashboard |

## Instruments (all prefixed `bb_`)

| Metric | Type | Labels |
|---|---|---|
| `bb_requests_total` | counter | `provider`, `outcome` (`ok` \| `upstream_error` \| `transport_error`) |
| `bb_request_duration_seconds` | histogram (buckets 0.05–60 s) | `provider` |
| `bb_tier_requests_total` | counter | `tier` (`local` \| `cloud` \| `static`) |
| `bb_escalations_total` | counter | `trigger` (`sentinel` \| `sticky` \| `fail_mode`) |
| `bb_budget_denied_total` | counter | — |
| `bb_cloud_budget_used` / `bb_cloud_budget_max` / `bb_sticky_conversations` | gauges | — |

## Proxy changes

- `src/metrics.rs` (new): `Metrics` struct owning a private `Registry` plus
  the instruments above; `new()`, `render() -> String` (text exposition),
  `observe_request(provider, outcome, elapsed)` helper; `OUTCOME_*` and
  `TIER_*` string constants. Unit-tested through `render()` output.
- `AppState` gains `metrics: Arc<Metrics>`, built in `build_state`.
- Instrumentation points:
  - `forward()`: duration timer + `bb_requests_total` outcome per provider
    (transport error / non-2xx / ok).
  - `local_attempt()`: same three outcomes for the local tier's own send.
  - `messages_proxy`: `tier=static` when the cascade is bypassed.
  - `cascade()`: `tier=local` on a local attempt; `escalate()`: `tier=cloud`
    plus `bb_escalations_total{trigger}` on grant, `bb_budget_denied_total`
    (and `tier=local`) on denial — incremented beside the existing
    `record_escalation` calls so panel and Grafana never disagree.
- `GET /metrics`: refreshes the three gauges from the orchestrator's
  existing readers (`budget_used()`, `sticky_count()`, cfg max), then
  serves the registry as `text/plain; version=0.0.4`. Read-only; never
  contacts upstream providers. Gauges stay 0 when the orchestrator is
  disabled.

## Docker stack changes

- `docker/prometheus.yml`: 15 s scrape of `big-brother:8787/metrics`.
- `docker/grafana/provisioning/datasources/prometheus.yml`: Prometheus
  datasource, `uid: prometheus`, default.
- `docker/grafana/provisioning/dashboards/provider.yml` +
  `docker/grafana/dashboards/big-brother.json`: file-provisioned dashboard —
  request rate by provider, p50/p95 latency, escalations by trigger, budget
  used-vs-max, sticky conversations, error rate.
- `docker-compose.yml`: services `prometheus` (`prom/prometheus`,
  `127.0.0.1:9090:9090`, `prometheus-data` volume) and `grafana`
  (`grafana/grafana`, `127.0.0.1:3001:3000`, admin password
  `${GRAFANA_ADMIN_PASSWORD:-admin}`, `grafana-data` volume). Networks keep
  the isolation discipline: new `metrics` network = {big-brother,
  prometheus} (Prometheus must scrape the proxy; it is a config-driven
  scraper whose config we commit); new `grafana` network = {prometheus,
  grafana}. Grafana can reach only Prometheus — never the proxy or Open
  WebUI.
- `.env.example` += `GRAFANA_ADMIN_PASSWORD=admin` (documented default,
  not a secret).

## Testing / verification

- Unit: registry renders all families; counters/histogram/labels appear
  correctly in `render()`.
- Integration (wiremock, existing harness): `/metrics` returns 200
  `text/plain` with gauge lines at 0 when the orchestrator is off; after
  one driven sentinel escalation the body contains
  `bb_escalations_total{trigger="sentinel"} 1`,
  `bb_tier_requests_total{tier="local"} 1` and `{tier="cloud"} 1`, the
  provider outcome counters, and `bb_cloud_budget_used 1`.
- Live: `docker compose up -d`; `curl localhost:8787/metrics` 200;
  Prometheus `api/v1/targets` reports the big-brother scrape `up`;
  `query?query=up{job="big-brother"}` returns 1; Grafana `api/health` ok at
  `localhost:3001`. (No live `/v1/messages` traffic is driven — the example
  LAN IP would hang until the request timeout.)

## Non-goals

GPU / LM-Studio host metrics (future LLM-control sub-project), alerting
rules, Grafana auth beyond its own login, and metrics persistence guarantees
beyond Prometheus's default retention.
