//! Cross-component Prometheus helpers shared by the `broker` and `runtime`
//! crates.
//!
//! Both processes expose a `/metrics` endpoint (D9) whose HTTP request rate /
//! latency histograms share the same shape — only the metric-name *prefix*
//! differs (`open_websandbox_broker_*` vs `open_websandbox_runtime_*`). This
//! module factors that common shape into one [`HttpMetrics`] holder so the
//! exposition format, the label order (`path`, `method`, `status`) and the
//! histogram buckets stay byte-identical across components.
//!
//! The `path` label is the **templated** matched route, supplied by each
//! component's own routing-aware middleware (axum 0.8 fills in path parameters,
//! so each crate normalises to a bounded template — see `broker::metrics` /
//! `runtime::metrics`). Raw URLs never reach the label set.
//!
//! Each process owns a *private* [`prometheus::Registry`] (carried on
//! `AppState`) rather than the crate's process-global default: that keeps
//! registration isolated per test (two `AppState`s in one test binary do not
//! panic on "duplicate collector") and makes exposition deterministic.

#![forbid(unsafe_code)]

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder,
};

/// Default Prometheus histogram buckets for HTTP request latency (seconds).
///
/// Issue #74 Q1 default: the canonical Prometheus latency buckets. Applied to
/// both the broker and runtime `*_http_request_duration_seconds` histograms.
pub const DEFAULT_HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// HTTP request rate + latency metrics for one component.
///
/// Owns an `open_websandbox_<prefix>_http_requests_total` counter and an
/// `open_websandbox_<prefix>_http_request_duration_seconds` histogram, both
/// labelled `{path, method, status}`. `prefix` is the bare component segment
/// (e.g. `"open_websandbox_broker"`) — the full metric names are derived from
/// it so they stay frozen on the Grafana dashboard's contract.
#[derive(Clone)]
pub struct HttpMetrics {
    requests_total: IntCounterVec,
    request_duration_seconds: HistogramVec,
}

impl HttpMetrics {
    /// Construct + register the HTTP rate/latency pair on `registry`.
    ///
    /// `prefix` is the frozen metric-name stem, e.g.
    /// `"open_websandbox_broker"` → emits
    /// `open_websandbox_broker_http_requests_total` and
    /// `open_websandbox_broker_http_request_duration_seconds`.
    pub fn new(prefix: &str, registry: &Registry) -> Self {
        let requests_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_http_requests_total"),
                "Total HTTP requests served, by templated path / method / status.",
            ),
            &["path", "method", "status"],
        )
        .expect("http_requests_total: valid label set");
        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                format!("{prefix}_http_request_duration_seconds"),
                "HTTP request latency in seconds, by templated path / method / status.",
            )
            .buckets(DEFAULT_HTTP_BUCKETS.to_vec()),
            &["path", "method", "status"],
        )
        .expect("http_request_duration_seconds: valid label set");
        registry
            .register(Box::new(requests_total.clone()))
            .expect("http_requests_total: freshly-created registry has no duplicates");
        registry
            .register(Box::new(request_duration_seconds.clone()))
            .expect("http_request_duration_seconds: freshly-created registry has no duplicates");
        Self {
            requests_total,
            request_duration_seconds,
        }
    }

    /// Record one served request: bump the counter + observe the latency.
    ///
    /// `path` MUST already be the bounded templated route (never the raw URL);
    /// `method` is the HTTP method; `status` is the response status code.
    pub fn observe(&self, path: &str, method: &str, status: u16, seconds: f64) {
        // `status` is formatted once per request — the cardinality of the set
        // is tiny, but the `prometheus` crate keys label values by string, so
        // the integer must be stringified. Allocation here is negligible
        // against the work already done serving the request.
        let status_str = status.to_string();
        let labels = [path, method, status_str.as_str()];
        self.requests_total.with_label_values(&labels).inc();
        self.request_duration_seconds
            .with_label_values(&labels)
            .observe(seconds);
    }
}

/// Render `registry`'s collectors in Prometheus text exposition format
/// (`text/plain; version=0.0.4`). Never returns an error: a failure to encode
/// yields an empty body rather than a broken scrape.
pub fn gather(registry: &Registry) -> String {
    let encoder = TextEncoder::new();
    let mut buf = Vec::<u8>::new();
    // Exposition is best-effort; never panic a /metrics scrape.
    let _ = encoder.encode(&registry.gather(), &mut buf);
    // Prometheus text exposition is ASCII/UTF-8; a malformed buffer would be a
    // crate bug, so fall back to empty rather than panicking the handler.
    String::from_utf8(buf).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_metrics_register_and_observe() {
        let registry = Registry::new();
        let m = HttpMetrics::new("open_websandbox_test", &registry);
        m.observe("/healthz", "GET", 200, 0.01);
        m.observe("/healthz", "GET", 200, 0.02);
        let out = gather(&registry);
        // Frozen metric names + label set appear verbatim.
        assert!(
            out.contains("open_websandbox_test_http_requests_total"),
            "missing counter name:\n{out}"
        );
        assert!(
            out.contains("open_websandbox_test_http_request_duration_seconds"),
            "missing histogram name:\n{out}"
        );
        // The observed label set materialises a series, with count == 2.
        // (prometheus sorts label values alphabetically: method,path,status.)
        assert!(
            out.contains(r#"method="GET",path="/healthz",status="200""#),
            "missing label set:\n{out}"
        );
        assert!(
            out.contains("open_websandbox_test_http_requests_total{method=\"GET\",path=\"/healthz\",status=\"200\"} 2"),
            "expected the counter series at value 2:\n{out}"
        );
    }

    #[test]
    fn fresh_registry_gather_is_empty() {
        let registry = Registry::new();
        assert_eq!(gather(&registry), "");
    }
}
