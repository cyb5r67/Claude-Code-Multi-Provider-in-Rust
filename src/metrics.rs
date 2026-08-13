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

pub const CHAT_MODE_PIPELINE: &str = "pipeline";
pub const CHAT_MODE_PASSTHROUGH: &str = "passthrough";

/// All proxy instruments, registered on one private registry.
pub struct Metrics {
    registry: Registry,
    pub requests_total: IntCounterVec,
    pub request_duration_seconds: HistogramVec,
    pub tier_requests_total: IntCounterVec,
    pub chat_requests_total: IntCounterVec,
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
        let chat_requests_total = IntCounterVec::new(
            Opts::new(
                "bb_chat_requests_total",
                "OpenAI-dialect chat requests by mode (pipeline vs passthrough)",
            ),
            &["mode"],
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
            .register(Box::new(chat_requests_total.clone()))
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
            chat_requests_total,
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
