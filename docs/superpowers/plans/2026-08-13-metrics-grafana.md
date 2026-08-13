# Metrics + Grafana Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prometheus metrics from the proxy at `GET /metrics` plus Prometheus and provisioned-Grafana containers in the Docker stack, landing on a working dashboard with zero manual setup.

**Architecture:** A `src/metrics.rs` module owns a `prometheus::Registry` and all instruments, carried in `AppState`; instrumentation lands at the existing seams (`forward`, `local_attempt`, `messages_proxy`, `cascade`, `escalate`). The compose stack gains `prometheus` (scrapes the proxy over a new `metrics` network) and `grafana` (reaches only Prometheus over a new `grafana` network), both localhost-published, with datasource + dashboard provisioned from committed files.

**Tech Stack:** Rust (`prometheus = "0.13"`, `default-features = false` — the only new dependency), `prom/prometheus` and `grafana/grafana` images.

**Spec:** `docs/superpowers/specs/2026-08-13-metrics-grafana-design.md`

## Global Constraints

- Only new Rust dependency: `prometheus = { version = "0.13", default-features = false }`.
- Metric names/labels exactly as the spec table: `bb_requests_total{provider, outcome}`, `bb_request_duration_seconds{provider}`, `bb_tier_requests_total{tier}`, `bb_escalations_total{trigger}`, `bb_budget_denied_total`, `bb_cloud_budget_used`, `bb_cloud_budget_max`, `bb_sticky_conversations`. Outcomes `ok|upstream_error|transport_error`; tiers `local|cloud|static`.
- The Prometheus text encoder may order labels alphabetically — tests must match series by checking name + each label + value on one line, never by assuming label order.
- `/metrics` is read-only, never contacts upstream providers, and serves `text/plain; version=0.0.4`. Gauges stay 0 with the orchestrator disabled.
- Metric increments sit beside the existing `record_escalation` calls (panel and Grafana must never disagree).
- Grafana publishes `127.0.0.1:3001:3000`; Prometheus `127.0.0.1:9090:9090`. Network isolation: `metrics` = {big-brother, prometheus}; `grafana` net = {prometheus, grafana}; Grafana must NOT share a network with big-brother or open-webui.
- No secrets in tracked files (`GRAFANA_ADMIN_PASSWORD=admin` in `.env.example` is a documented default, overridable in `.env`).
- `cargo fmt` before every commit; `cargo test` green after every task; pre-existing tests untouched.

---

### Task 1: `metrics` module and `AppState` wiring

**Files:**
- Modify: `Cargo.toml`
- Create: `src/metrics.rs`
- Modify: `src/lib.rs`
- Modify: `src/proxy.rs` (AppState field + `/metrics` route/handler only — no instrumentation yet)
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Produces: `metrics::Metrics` with `new()`, `render() -> String`, `observe_request(&self, provider: &str, outcome: &str, elapsed: std::time::Duration)`, public instrument fields `requests_total: IntCounterVec`, `request_duration_seconds: HistogramVec`, `tier_requests_total: IntCounterVec`, `escalations_total: IntCounterVec`, `budget_denied_total: IntCounter`, `cloud_budget_used: IntGauge`, `cloud_budget_max: IntGauge`, `sticky_conversations: IntGauge`; constants `OUTCOME_OK`, `OUTCOME_UPSTREAM_ERROR`, `OUTCOME_TRANSPORT_ERROR`, `TIER_LOCAL`, `TIER_CLOUD`, `TIER_STATIC`. `AppState.metrics: Arc<Metrics>`. Route `GET /metrics`.

- [ ] **Step 1: Add the dependency** — in `Cargo.toml` `[dependencies]`:

```toml
prometheus = { version = "0.13", default-features = false }
```

- [ ] **Step 2: Create `src/metrics.rs`:**

```rust
//! Prometheus instrumentation for the proxy.

use std::time::Duration;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

pub const OUTCOME_OK: &str = "ok";
pub const OUTCOME_UPSTREAM_ERROR: &str = "upstream_error";
pub const OUTCOME_TRANSPORT_ERROR: &str = "transport_error";

pub const TIER_LOCAL: &str = "local";
pub const TIER_CLOUD: &str = "cloud";
pub const TIER_STATIC: &str = "static";

/// All proxy instruments, registered on one private registry.
pub struct Metrics {
    registry: Registry,
    pub requests_total: IntCounterVec,
    pub request_duration_seconds: HistogramVec,
    pub tier_requests_total: IntCounterVec,
    pub escalations_total: IntCounterVec,
    pub budget_denied_total: IntCounter,
    pub cloud_budget_used: IntGauge,
    pub cloud_budget_max: IntGauge,
    pub sticky_conversations: IntGauge,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new(
                "bb_requests_total",
                "Upstream requests by provider and outcome",
            ),
            &["provider", "outcome"],
        )
        .expect("valid metric");
        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "bb_request_duration_seconds",
                "Upstream request duration by provider",
            )
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]),
            &["provider"],
        )
        .expect("valid metric");
        let tier_requests_total = IntCounterVec::new(
            Opts::new(
                "bb_tier_requests_total",
                "Requests dispatched per tier (a budget-denied fallback dispatches local twice)",
            ),
            &["tier"],
        )
        .expect("valid metric");
        let escalations_total = IntCounterVec::new(
            Opts::new("bb_escalations_total", "Granted escalations by trigger"),
            &["trigger"],
        )
        .expect("valid metric");
        let budget_denied_total = IntCounter::new(
            "bb_budget_denied_total",
            "Escalations denied by the hourly cloud budget",
        )
        .expect("valid metric");
        let cloud_budget_used = IntGauge::new(
            "bb_cloud_budget_used",
            "Cloud budget reservations in the sliding hour",
        )
        .expect("valid metric");
        let cloud_budget_max =
            IntGauge::new("bb_cloud_budget_max", "Configured hourly cloud budget cap")
                .expect("valid metric");
        let sticky_conversations = IntGauge::new(
            "bb_sticky_conversations",
            "Conversations currently sticky to the cloud tier",
        )
        .expect("valid metric");

        registry
            .register(Box::new(requests_total.clone()))
            .expect("register");
        registry
            .register(Box::new(request_duration_seconds.clone()))
            .expect("register");
        registry
            .register(Box::new(tier_requests_total.clone()))
            .expect("register");
        registry
            .register(Box::new(escalations_total.clone()))
            .expect("register");
        registry
            .register(Box::new(budget_denied_total.clone()))
            .expect("register");
        registry
            .register(Box::new(cloud_budget_used.clone()))
            .expect("register");
        registry
            .register(Box::new(cloud_budget_max.clone()))
            .expect("register");
        registry
            .register(Box::new(sticky_conversations.clone()))
            .expect("register");

        Metrics {
            registry,
            requests_total,
            request_duration_seconds,
            tier_requests_total,
            escalations_total,
            budget_denied_total,
            cloud_budget_used,
            cloud_budget_max,
            sticky_conversations,
        }
    }

    /// Count one upstream request and observe its duration.
    pub fn observe_request(&self, provider: &str, outcome: &str, elapsed: Duration) {
        self.requests_total
            .with_label_values(&[provider, outcome])
            .inc();
        self.request_duration_seconds
            .with_label_values(&[provider])
            .observe(elapsed.as_secs_f64());
    }

    /// Prometheus text exposition of every registered instrument.
    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("text encoding cannot fail");
        String::from_utf8(buf).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when a series line for `name` carries every given label pair and
    /// the value — label ORDER is encoder-defined, so never match on it.
    fn has_series(text: &str, name: &str, labels: &[(&str, &str)], value: &str) -> bool {
        text.lines().any(|l| {
            l.starts_with(name)
                && labels
                    .iter()
                    .all(|(k, v)| l.contains(&format!(r#"{k}="{v}""#)))
                && l.ends_with(&format!(" {value}"))
        })
    }

    #[test]
    fn plain_instruments_render_at_zero() {
        let text = Metrics::new().render();
        assert!(text.contains("bb_budget_denied_total 0"));
        assert!(text.contains("bb_cloud_budget_used 0"));
        assert!(text.contains("bb_cloud_budget_max 0"));
        assert!(text.contains("bb_sticky_conversations 0"));
    }

    #[test]
    fn observe_request_records_counter_and_histogram() {
        let m = Metrics::new();
        m.observe_request("qwen", OUTCOME_OK, Duration::from_millis(120));
        let text = m.render();
        assert!(has_series(
            &text,
            "bb_requests_total",
            &[("provider", "qwen"), ("outcome", "ok")],
            "1"
        ));
        assert!(has_series(
            &text,
            "bb_request_duration_seconds_bucket",
            &[("provider", "qwen"), ("le", "0.25")],
            "1"
        ));
        assert!(has_series(
            &text,
            "bb_request_duration_seconds_count",
            &[("provider", "qwen")],
            "1"
        ));
    }

    #[test]
    fn labeled_counters_render_expected_series() {
        let m = Metrics::new();
        m.tier_requests_total.with_label_values(&[TIER_LOCAL]).inc();
        m.escalations_total.with_label_values(&["sentinel"]).inc();
        m.budget_denied_total.inc();
        let text = m.render();
        assert!(has_series(
            &text,
            "bb_tier_requests_total",
            &[("tier", "local")],
            "1"
        ));
        assert!(has_series(
            &text,
            "bb_escalations_total",
            &[("trigger", "sentinel")],
            "1"
        ));
        assert!(text.contains("bb_budget_denied_total 1"));
    }
}
```

- [ ] **Step 3: Register the module and wire state** — `src/lib.rs`: add `pub mod metrics;` (alphabetical: config, error, metrics, model_command, orchestrator, proxy, stream). In `build_state`, construct and store it:

```rust
    Ok(AppState {
        config: Arc::new(config),
        client,
        orchestrator,
        metrics: Arc::new(crate::metrics::Metrics::new()),
    })
```

In `src/proxy.rs`, add the field and the endpoint:

```rust
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
    pub orchestrator: Option<Arc<Orchestrator>>,
    pub metrics: Arc<Metrics>,
}
```

```rust
/// Prometheus text exposition. Read-only: refreshes gauges from in-memory
/// state and never contacts upstream providers.
async fn metrics_endpoint(State(state): State<AppState>) -> Response {
    if let Some(orch) = &state.orchestrator {
        state.metrics.cloud_budget_used.set(orch.budget_used() as i64);
        state
            .metrics
            .cloud_budget_max
            .set(orch.cfg.max_cloud_requests_per_hour as i64);
        state
            .metrics
            .sticky_conversations
            .set(orch.sticky_count() as i64);
    }
    let mut response = Response::new(Body::from(state.metrics.render()));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, "text/plain; version=0.0.4".parse().unwrap());
    response.into_response()
}
```

Route in `router()`: `.route("/metrics", get(metrics_endpoint))`.

- [ ] **Step 4: Integration test** — append to `tests/proxy_integration.rs`:

```rust
#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let server = MockServer::start().await;
    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let app = proxy::router(build_state(cfg).unwrap());

    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/plain"));
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    // Orchestrator disabled: gauges exist and read zero.
    assert!(text.contains("bb_cloud_budget_used 0"));
    assert!(text.contains("bb_sticky_conversations 0"));
    assert!(text.contains("bb_budget_denied_total 0"));
}
```

- [ ] **Step 5: Verify**

Run: `cargo test`
Expected: all pass (existing 97 + 3 unit + 1 integration).

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/metrics.rs src/lib.rs src/proxy.rs tests/proxy_integration.rs
git commit -m "Add Prometheus metrics registry and GET /metrics"
```

---

### Task 2: Instrumentation at the routing seams

**Files:**
- Modify: `src/proxy.rs`
- Test: `tests/proxy_integration.rs`

**Interfaces:**
- Consumes: Task 1's `Metrics` fields/constants via `state.metrics`.
- Produces: fully instrumented request flow (spec §Proxy changes).

- [ ] **Step 1: Write the failing integration test** — append to `tests/proxy_integration.rs`:

```rust
/// True when a series line for `name` carries every label pair and the value.
fn has_series(text: &str, name: &str, labels: &[(&str, &str)], value: &str) -> bool {
    text.lines().any(|l| {
        l.starts_with(name)
            && labels
                .iter()
                .all(|(k, v)| l.contains(&format!(r#"{k}="{v}""#)))
            && l.ends_with(&format!(" {value}"))
    })
}

async fn get_text(app: axum::Router, uri: &str) -> String {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn metrics_reflect_a_sentinel_escalation() {
    let local = MockServer::start().await;
    let cloud = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sentinel_json_response()))
        .expect(1)
        .mount(&local)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"routed": "cloud"})))
        .expect(1)
        .mount(&cloud)
        .await;

    let cfg =
        Config::from_toml_str(&orchestrated_config_toml(&local.uri(), &cloud.uri(), 10, "cloud"))
            .unwrap();
    let state = build_state(cfg).unwrap();

    let (s1, _) = send(
        proxy::router(state.clone()),
        json!({"model": "m", "messages": [{"role": "user", "content": "hard question"}]}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    let text = get_text(proxy::router(state), "/metrics").await;
    assert!(has_series(&text, "bb_tier_requests_total", &[("tier", "local")], "1"));
    assert!(has_series(&text, "bb_tier_requests_total", &[("tier", "cloud")], "1"));
    assert!(has_series(
        &text,
        "bb_escalations_total",
        &[("trigger", "sentinel")],
        "1"
    ));
    assert!(has_series(
        &text,
        "bb_requests_total",
        &[("provider", "local"), ("outcome", "ok")],
        "1"
    ));
    assert!(has_series(
        &text,
        "bb_requests_total",
        &[("provider", "cloud"), ("outcome", "ok")],
        "1"
    ));
    assert!(text.contains("bb_cloud_budget_used 1"));
    assert!(text.contains("bb_sticky_conversations 1"));
}

#[tokio::test]
async fn metrics_count_static_routing_and_upstream_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = Config::from_toml_str(&config_toml(&server.uri(), &server.uri())).unwrap();
    let state = build_state(cfg).unwrap();

    let (status, _) = send(
        proxy::router(state.clone()),
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let text = get_text(proxy::router(state), "/metrics").await;
    assert!(has_series(&text, "bb_tier_requests_total", &[("tier", "static")], "1"));
    assert!(has_series(
        &text,
        "bb_requests_total",
        &[("provider", "primary"), ("outcome", "upstream_error")],
        "1"
    ));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test proxy_integration metrics_`
Expected: `metrics_endpoint_serves_prometheus_text` passes; the two new tests FAIL (no series recorded yet).

- [ ] **Step 3: Instrument `src/proxy.rs`.** Add the import:

```rust
use crate::metrics;
```

(Task 1 already imports `crate::metrics::Metrics` for the AppState field; keep both — `metrics::OUTCOME_*` constants read better fully qualified.)

**`forward()`** — restructure the send to capture all three outcomes with one timer:

```rust
    let started = std::time::Instant::now();
    let upstream = match apply_auth(
        state.client.post(&provider.base_url),
        provider.auth_style,
        &api_key,
    )
    .json(payload)
    .send()
    .await
    {
        Ok(resp) => resp,
        Err(source) => {
            state.metrics.observe_request(
                provider_key,
                metrics::OUTCOME_TRANSPORT_ERROR,
                started.elapsed(),
            );
            return Err(AppError::Upstream {
                provider: provider_key.to_string(),
                source,
            });
        }
    };
```

In its non-success branch, before building the response:

```rust
        state.metrics.observe_request(
            provider_key,
            metrics::OUTCOME_UPSTREAM_ERROR,
            started.elapsed(),
        );
```

And immediately before the success-path streaming return:

```rust
    state
        .metrics
        .observe_request(provider_key, metrics::OUTCOME_OK, started.elapsed());
```

**`local_attempt()`** — same pattern around its own send: timer before `apply_auth`, `OUTCOME_TRANSPORT_ERROR` in the send-error arm, `OUTCOME_UPSTREAM_ERROR` in the non-success branch, and one `OUTCOME_OK` observation right after the status/content-type are read on the success path (before the SSE/JSON branch, so Clean and Escalate both count the local request as ok).

**`messages_proxy`** — in the fall-through (non-cascade) path, immediately before `forward(...)`:

```rust
    state
        .metrics
        .tier_requests_total
        .with_label_values(&[metrics::TIER_STATIC])
        .inc();
```

**`cascade()`** — immediately before calling `local_attempt`:

```rust
    state
        .metrics
        .tier_requests_total
        .with_label_values(&[metrics::TIER_LOCAL])
        .inc();
```

**`escalate()`** — in the budget-denied branch, beside the existing `record_escalation("budget_denied", ...)`:

```rust
        state.metrics.budget_denied_total.inc();
        state
            .metrics
            .tier_requests_total
            .with_label_values(&[metrics::TIER_LOCAL])
            .inc();
```

In the granted path, beside the existing `record_escalation(trigger, ...)`:

```rust
    state
        .metrics
        .escalations_total
        .with_label_values(&[trigger])
        .inc();
    state
        .metrics
        .tier_requests_total
        .with_label_values(&[metrics::TIER_CLOUD])
        .inc();
```

- [ ] **Step 4: Verify**

Run: `cargo test`
Expected: everything green, including both new tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/proxy.rs tests/proxy_integration.rs
git commit -m "Instrument routing, cascade, and escalation with Prometheus metrics"
```

---

### Task 3: Prometheus + Grafana in the compose stack

**Files:**
- Create: `docker/prometheus.yml`
- Create: `docker/grafana/provisioning/datasources/prometheus.yml`
- Create: `docker/grafana/provisioning/dashboards/provider.yml`
- Create: `docker/grafana/dashboards/big-brother.json`
- Modify: `docker-compose.yml`
- Modify: `.env.example`

- [ ] **Step 1: `docker/prometheus.yml`:**

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: big-brother
    metrics_path: /metrics
    static_configs:
      - targets: ["big-brother:8787"]
```

- [ ] **Step 2: `docker/grafana/provisioning/datasources/prometheus.yml`:**

```yaml
apiVersion: 1
datasources:
  - name: Prometheus
    uid: prometheus
    type: prometheus
    access: proxy
    url: http://prometheus:9090
    isDefault: true
```

- [ ] **Step 3: `docker/grafana/provisioning/dashboards/provider.yml`:**

```yaml
apiVersion: 1
providers:
  - name: big-brother
    type: file
    options:
      path: /etc/grafana/dashboards
```

- [ ] **Step 4: `docker/grafana/dashboards/big-brother.json`:**

```json
{
  "uid": "big-brother",
  "title": "Big Brother",
  "schemaVersion": 39,
  "version": 1,
  "refresh": "10s",
  "time": { "from": "now-1h", "to": "now" },
  "panels": [
    {
      "id": 1,
      "type": "timeseries",
      "title": "Request rate by provider",
      "gridPos": { "x": 0, "y": 0, "w": 12, "h": 8 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": { "unit": "reqps" }, "overrides": [] },
      "targets": [
        {
          "refId": "A",
          "expr": "sum by (provider) (rate(bb_requests_total[5m]))",
          "legendFormat": "{{provider}}"
        }
      ]
    },
    {
      "id": 2,
      "type": "timeseries",
      "title": "Latency p50 / p95",
      "gridPos": { "x": 12, "y": 0, "w": 12, "h": 8 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": { "unit": "s" }, "overrides": [] },
      "targets": [
        {
          "refId": "A",
          "expr": "histogram_quantile(0.5, sum by (le, provider) (rate(bb_request_duration_seconds_bucket[5m])))",
          "legendFormat": "p50 {{provider}}"
        },
        {
          "refId": "B",
          "expr": "histogram_quantile(0.95, sum by (le, provider) (rate(bb_request_duration_seconds_bucket[5m])))",
          "legendFormat": "p95 {{provider}}"
        }
      ]
    },
    {
      "id": 3,
      "type": "timeseries",
      "title": "Escalations by trigger (15m)",
      "gridPos": { "x": 0, "y": 8, "w": 8, "h": 8 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": {}, "overrides": [] },
      "targets": [
        {
          "refId": "A",
          "expr": "sum by (trigger) (increase(bb_escalations_total[15m]))",
          "legendFormat": "{{trigger}}"
        },
        {
          "refId": "B",
          "expr": "increase(bb_budget_denied_total[15m])",
          "legendFormat": "budget_denied"
        }
      ]
    },
    {
      "id": 4,
      "type": "stat",
      "title": "Cloud budget (last hour)",
      "gridPos": { "x": 8, "y": 8, "w": 8, "h": 8 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": {}, "overrides": [] },
      "targets": [
        { "refId": "A", "expr": "bb_cloud_budget_used", "legendFormat": "used" },
        { "refId": "B", "expr": "bb_cloud_budget_max", "legendFormat": "max" }
      ]
    },
    {
      "id": 5,
      "type": "stat",
      "title": "Sticky cloud conversations",
      "gridPos": { "x": 16, "y": 8, "w": 8, "h": 4 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": {}, "overrides": [] },
      "targets": [
        { "refId": "A", "expr": "bb_sticky_conversations", "legendFormat": "sticky" }
      ]
    },
    {
      "id": 6,
      "type": "timeseries",
      "title": "Error rate by provider",
      "gridPos": { "x": 16, "y": 12, "w": 8, "h": 4 },
      "datasource": { "type": "prometheus", "uid": "prometheus" },
      "fieldConfig": { "defaults": { "unit": "reqps" }, "overrides": [] },
      "targets": [
        {
          "refId": "A",
          "expr": "sum by (provider) (rate(bb_requests_total{outcome!=\"ok\"}[5m]))",
          "legendFormat": "{{provider}}"
        }
      ]
    }
  ]
}
```

- [ ] **Step 5: Extend `docker-compose.yml`.** Add `metrics` to big-brother's networks (keeping `proxy`):

```yaml
    networks: [proxy, metrics]
```

Append the two services:

```yaml
  prometheus:
    image: prom/prometheus:latest
    ports:
      - "127.0.0.1:9090:9090"
    volumes:
      - ./docker/prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - prometheus-data:/prometheus
    networks: [metrics, grafana]
    restart: unless-stopped

  grafana:
    image: grafana/grafana:latest
    ports:
      - "127.0.0.1:3001:3000" # 3000 is taken by open-webui on the host
    environment:
      GF_SECURITY_ADMIN_PASSWORD: "${GRAFANA_ADMIN_PASSWORD:-admin}"
    volumes:
      - ./docker/grafana/provisioning:/etc/grafana/provisioning:ro
      - ./docker/grafana/dashboards:/etc/grafana/dashboards:ro
      - grafana-data:/var/lib/grafana
    networks: [grafana] # reaches ONLY prometheus — never the proxy or open-webui
    restart: unless-stopped
```

Extend the top-level maps:

```yaml
networks:
  proxy: {}
  webui: {}
  metrics: {}
  grafana: {}

volumes:
  open-webui-data:
  prometheus-data:
  grafana-data:
```

- [ ] **Step 6: `.env.example`** — append:

```
# Grafana admin login (user "admin"). This default is documented, not secret;
# override it in .env if the dashboard matters to you.
GRAFANA_ADMIN_PASSWORD=admin
```

- [ ] **Step 7: Verify**

Run: `docker compose config --quiet`
Expected: exit 0, silent.

- [ ] **Step 8: Commit**

```bash
git add docker/prometheus.yml docker/grafana docker-compose.yml .env.example
git commit -m "Add Prometheus and provisioned Grafana to the compose stack"
```

---

### Task 4: Docs and live verification

**Files:**
- Modify: `README.md`
- Modify: `docs/USER_GUIDE.md`

- [ ] **Step 1: README** — extend the Docker paragraph in Usage ("Or with Docker" block) by appending to its prose:

```markdown
Prometheus runs at <http://localhost:9090> and a pre-provisioned Grafana
dashboard at <http://localhost:3001> (login `admin` /
`GRAFANA_ADMIN_PASSWORD`).
```

- [ ] **Step 2: USER_GUIDE** — in the "Running with Docker" section's service table, add rows:

```markdown
| `prometheus`  | <http://localhost:9090> | Scrapes the proxy's `/metrics` every 15 s |
| `grafana`     | <http://localhost:3001> | Dashboard over Prometheus (`admin` / `GRAFANA_ADMIN_PASSWORD`) |
```

After the table's bullet list, add:

```markdown
- **Metrics:** the proxy exposes Prometheus text format at
  `http://localhost:8787/metrics` (requests by provider/outcome, latency
  histograms, tier dispatches, escalations by trigger, budget and sticky
  gauges). The bundled Grafana dashboard ("Big Brother") is provisioned
  from `docker/grafana/dashboards/big-brother.json` — edit the JSON and
  restart Grafana to change it. History lives in the `prometheus-data`
  volume (default retention).
```

- [ ] **Step 3: Live verification** (stack from the docker-stack feature is running; recreate with the new services):

```bash
docker compose up -d
docker compose ps
```

Expected: four services Up, big-brother healthy.

```bash
curl -s http://localhost:8787/metrics | head -5
curl -s "http://localhost:9090/api/v1/targets" | grep -o '"health":"up"' | head -1
curl -s "http://localhost:9090/api/v1/query?query=up%7Bjob%3D%22big-brother%22%7D" | grep -o '"value":\[[^]]*\]'
curl -s http://localhost:3001/api/health
```

Expected: `bb_` metric text; `"health":"up"` (allow ~30 s after start for the first scrape; retry); the query value ends with `"1"`; Grafana health JSON with `"database": "ok"` (Grafana may take ~30 s; retry).

Then `cargo fmt --check && cargo test` natively — green.

Leave the stack running.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/USER_GUIDE.md
git commit -m "Document the metrics endpoint and Grafana dashboard"
```
