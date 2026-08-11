//! Cross-component Prometheus metrics helpers shared by the `broker` and
//! `runtime` crates (D9, issue #74 Q4 → `metrics` facade).
//!
//! Both processes expose a `/metrics` endpoint whose HTTP request rate /
//! latency histograms share the same shape — only the metric-name *prefix*
//! differs (`open_websandbox_broker_*` vs `open_websandbox_runtime_*`). This
//! module installs the single `metrics-exporter-prometheus` recorder per
//! process and factors the common HTTP rate/latency pair into one
//! [`HttpMetrics`] holder so the exposition format, the label order
//! (`path`, `method`, `status`) and the histogram buckets stay byte-identical
//! across components.
//!
//! Recording goes through the `metrics` facade: call sites use
//! `metrics::counter!` / `metrics::histogram!` / `metrics::gauge!` against the
//! process-global recorder installed by [`install`]. The `path` label is the
//! **templated** matched route (bounded cardinality, never the raw URL).

#![forbid(unsafe_code)]

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Default Prometheus histogram buckets for HTTP request latency (seconds).
///
/// Issue #74 Q1 default: the canonical Prometheus latency buckets. Configured
/// once on the [`PrometheusBuilder`] so every latency histogram — broker and
/// runtime `*_http_request_duration_seconds` — shares them.
pub const DEFAULT_HTTP_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the single `metrics-exporter-prometheus` recorder for this process
/// (idempotent) and return its [`PrometheusHandle`].
///
/// The recorder is built once with [`DEFAULT_HTTP_BUCKETS`] for every latency
/// histogram. Safe to call any number of times — e.g. from each
/// `*Metrics::new()` *and* again from `/metrics` — the first call installs,
/// the rest reuse the cached handle (the global recorder may only be set
/// once, hence the [`OnceLock`]).
#[must_use]
pub fn install() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .set_buckets(DEFAULT_HTTP_BUCKETS)
                .expect("default histogram buckets are finite + sorted")
                .install_recorder()
                .expect("install Prometheus exporter recorder")
        })
        .clone()
}

/// Render every registered metric in Prometheus text exposition format
/// (`text/plain; version=0.0.4`). Installs the recorder on first use.
pub fn gather() -> String {
    install().render()
}

/// HTTP request rate + latency metrics for one component (facade-backed).
///
/// Owns the `open_websandbox_<prefix>_http_requests_total` counter and the
/// `open_websandbox_<prefix>_http_request_duration_seconds` histogram, both
/// labelled `{path, method, status}`. The full metric names are supplied as
/// `&'static str` (taken from the constructing crate's frozen-name consts)
/// so they stay byte-identical to the Grafana dashboard contract.
#[derive(Clone)]
pub struct HttpMetrics {
    requests_total_name: &'static str,
    request_duration_seconds_name: &'static str,
}

impl HttpMetrics {
    /// Construct the HTTP rate/latency pair against the global recorder.
    ///
    /// `requests_total_name` / `request_duration_seconds_name` are the frozen
    /// full metric names, e.g. `"open_websandbox_broker_http_requests_total"`
    /// and `"open_websandbox_broker_http_request_duration_seconds"`.
    #[must_use]
    pub fn new(
        requests_total_name: &'static str,
        request_duration_seconds_name: &'static str,
    ) -> Self {
        // Ensure the global recorder is installed before describing / recording so
        // this holder is self-contained (broker/runtime also install via their
        // `*Metrics::new`; both paths are idempotent).
        let _ = install();
        metrics::describe_counter!(
            requests_total_name,
            "Total HTTP requests served, by templated path / method / status."
        );
        metrics::describe_histogram!(
            request_duration_seconds_name,
            "HTTP request latency in seconds, by templated path / method / status."
        );
        Self {
            requests_total_name,
            request_duration_seconds_name,
        }
    }

    /// Record one served request: bump the counter + observe the latency.
    ///
    /// `path` MUST already be the bounded templated route (never the raw URL);
    /// `method` is the HTTP method; `status` is the response status code.
    pub fn observe(&self, path: &str, method: &str, status: u16, seconds: f64) {
        // The `metrics` macros key label values by `SharedString<'static>`, so the
        // request-scoped values are owned once here (tiny strings; dwarfed by the
        // actual request work) and reused for the counter + histogram.
        let labels = [
            ("path", path.to_string()),
            ("method", method.to_string()),
            ("status", status.to_string()),
        ];
        metrics::counter!(self.requests_total_name, &labels).increment(1);
        metrics::histogram!(self.request_duration_seconds_name, &labels).record(seconds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The global recorder is shared across this test binary; these `test_*`
    // metric names are unique to this module so the observed counter value is
    // deterministic at 2 regardless of concurrent tests.
    const TEST_REQUESTS_TOTAL: &str = "open_websandbox_test_http_requests_total";
    const TEST_REQUEST_DURATION: &str = "open_websandbox_test_http_request_duration_seconds";

    #[test]
    fn http_metrics_register_and_observe() {
        let m = HttpMetrics::new(TEST_REQUESTS_TOTAL, TEST_REQUEST_DURATION);
        m.observe("/healthz", "GET", 200, 0.01);
        m.observe("/healthz", "GET", 200, 0.02);
        let out = gather();
        // Frozen metric names + label set appear verbatim.
        assert!(
            out.contains(TEST_REQUESTS_TOTAL),
            "missing counter name:\n{out}"
        );
        assert!(
            out.contains(TEST_REQUEST_DURATION),
            "missing histogram name:\n{out}"
        );
        // The exporter preserves the label insertion order (path, method, status).
        assert!(
            out.contains(r#"path="/healthz",method="GET",status="200""#),
            "missing label set:\n{out}"
        );
        assert!(
            out.contains(&format!(
                r#"{TEST_REQUESTS_TOTAL}{{path="/healthz",method="GET",status="200"}} 2"#
            )),
            "expected the counter series at value 2:\n{out}"
        );
    }

    #[test]
    fn install_is_idempotent_and_returns_a_handle() {
        // Calling install() repeatedly must not panic (single global recorder)
        // and must return handles that render the same catalogue. Seed a series
        // unique to this test so the assertion is self-contained (independent of
        // parallel test ordering).
        const IDEMPOTENT_TOTAL: &str = "open_websandbox_test_idempotent_install_total";
        let a = install();
        metrics::counter!(IDEMPOTENT_TOTAL).increment(7);
        let body_a = a.render();
        let body_b = install().render();
        assert!(
            body_a.contains(IDEMPOTENT_TOTAL),
            "idempotent handle should expose the seeded series:\\n{body_a}"
        );
        assert!(
            body_b.contains(IDEMPOTENT_TOTAL),
            "repeated install() must expose the seeded series:\\n{body_b}"
        );
        // Both handles share the single global recorder, so they must agree on
        // this test's own series (we avoid asserting full-body equality, which
        // is non-deterministic under parallel tests that share the recorder).
        assert_eq!(
            body_a.contains(IDEMPOTENT_TOTAL),
            body_b.contains(IDEMPOTENT_TOTAL)
        );
    }
}
