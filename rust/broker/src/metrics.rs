//! D9 — Prometheus metrics for the broker: the registry + non-HTTP gauges /
//! counters, the templated-route label normaliser, and the axum HTTP
//! middleware that records rate / latency.
//!
//! ## Metric catalogue (frozen names — Grafana dashboard contract)
//!
//! HTTP (one series per `{path, method, status}`):
//! - `open_websandbox_broker_http_requests_total` (counter)
//! - `open_websandbox_broker_http_request_duration_seconds` (histogram)
//!
//! Lifecycle:
//! - `open_websandbox_broker_active_sandboxes` (gauge; updated each leader
//!   reaper tick — reflects the broker-owned set the elected leader sees)
//! - `open_websandbox_broker_sandboxes_created_total` (counter)
//! - `open_websandbox_broker_sandboxes_deleted_total` (counter)
//! - `open_websandbox_broker_runtime_hop_errors_total` (counter)
//!
//! The HTTP counter / histogram live in [`shared::HttpMetrics`]; everything
//! here is broker-specific. The `path` label is the **templated** matched
//! route (bounded cardinality) — see [`route_label`].

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use prometheus::{IntCounter, IntGauge, Opts, Registry};
use shared::HttpMetrics;
use tracing::Instrument;

use crate::state::AppState;

/// Frozen metric-name stem for every broker metric.
pub const PREFIX: &str = "open_websandbox_broker";

/// All broker Prometheus collectors + the registry that owns them.
///
/// One instance lives on [`AppState`] (shared by every handler + the reaper
/// background task) so a single scrape gathers the full catalogue.
#[derive(Clone)]
pub struct BrokerMetrics {
    pub registry: Registry,
    pub http: HttpMetrics,
    pub active_sandboxes: IntGauge,
    pub sandboxes_created_total: IntCounter,
    pub sandboxes_deleted_total: IntCounter,
    pub runtime_hop_errors_total: IntCounter,
}

impl BrokerMetrics {
    /// Construct + register the full broker catalogue on a fresh registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();
        let http = HttpMetrics::new(PREFIX, &registry);
        let active_sandboxes = IntGauge::with_opts(Opts::new(
            format!("{PREFIX}_active_sandboxes"),
            "Broker-owned sandboxes currently observed by the elected leader \
             (updated each leader reaper tick).",
        ))
        .expect("active_sandboxes: valid opts");
        let sandboxes_created_total = IntCounter::with_opts(Opts::new(
            format!("{PREFIX}_sandboxes_created_total"),
            "Sandboxes created via the broker (resolve get-or-create + explicit \
             POST /api/sandboxes).",
        ))
        .expect("sandboxes_created_total: valid opts");
        let sandboxes_deleted_total = IntCounter::with_opts(Opts::new(
            format!("{PREFIX}_sandboxes_deleted_total"),
            "Sandboxes deleted via the broker (reaper reap + explicit DELETE \
             /api/sandboxes/{name}).",
        ))
        .expect("sandboxes_deleted_total: valid opts");
        let runtime_hop_errors_total = IntCounter::with_opts(Opts::new(
            format!("{PREFIX}_runtime_hop_errors_total"),
            "Reverse-proxy hops to a resolved runtime pod that failed \
             (transport / connect / send errors).",
        ))
        .expect("runtime_hop_errors_total: valid opts");
        registry
            .register(Box::new(active_sandboxes.clone()))
            .expect("fresh registry has no duplicate collectors");
        registry
            .register(Box::new(sandboxes_created_total.clone()))
            .expect("fresh registry has no duplicate collectors");
        registry
            .register(Box::new(sandboxes_deleted_total.clone()))
            .expect("fresh registry has no duplicate collectors");
        registry
            .register(Box::new(runtime_hop_errors_total.clone()))
            .expect("fresh registry has no duplicate collectors");
        Arc::new(Self {
            registry,
            http,
            active_sandboxes,
            sandboxes_created_total,
            sandboxes_deleted_total,
            runtime_hop_errors_total,
        })
    }
}

impl Default for BrokerMetrics {
    fn default() -> Self {
        // `Arc::unwrap_or_clone` keeps the call site simple while `new` returns
        // an `Arc`; tests construct a `BrokerMetrics` directly via this default.
        let arc = Self::new();
        Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone())
    }
}

/// Normalise a matched path into a **bounded** `path` label.
///
/// axum 0.8 fills path parameters into `MatchedPath` (`/api/sandboxes/foo`,
/// not the `/api/sandboxes/{name}` template), so we map every parametric route
/// back to its template and fold the catch-all reverse proxy + the WS relay
/// into fixed labels (issue #74 Q5):
///
/// - static open / gated routes (`/healthz`, `/api/sandboxes`, …) → verbatim;
/// - `/api/sandboxes/{name}` → the `{name}` template;
/// - `GET /api/terminals/{id}` (WS relay) → `terminal_ws`;
/// - everything else (the `/{*path}` catch-all: `/execute`, `/files/*`,
///   `/snapshot`, `/restore`, `POST /api/terminals`, …) → `proxy`.
///
/// The broker registers a catch-all, so every request matches *some* route —
/// this never sees an unbounded raw URL.
#[must_use]
pub fn route_label(matched: &str) -> &str {
    match matched {
        "/healthz" | "/readyz" | "/metrics" | "/openapi.json" | "/docs" | "/api/config"
        | "/api/status" | "/api/sandboxes" => matched,
        _ if matched.starts_with("/api/sandboxes/") => "/api/sandboxes/{name}",
        _ if matched.starts_with("/api/terminals/") => "terminal_ws",
        _ => "proxy",
    }
}

/// axum `from_fn` middleware: record request rate + latency for every served
/// request, keyed by the templated route (via [`route_label`]).
pub async fn http_metrics_layer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = request.method().as_str().to_owned();
    // axum inserts `MatchedPath` into request extensions during routing, before
    // this layer's service runs. For parametric / catch-all routes it is the
    // path with parameters filled in.
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned());
    // Broker-side fallback: the catch-all matches every path, so even without
    // the extension the raw URI path normalises to a bounded label.
    let raw_path = request.uri().path().to_owned();
    let path_label = match matched.as_deref() {
        Some(m) => route_label(m),
        None => route_label(&raw_path),
    };
    // D9 — OTel: one span per served request (templated route + method + status).
    // When the OTLP bridge is active (OTEL_EXPORTER_OTLP_ENDPOINT set) this
    // becomes an exported span; otherwise it is a cheap no-op and adds no log
    // noise (the fmt layer does not log span lifecycle by default).
    let span = tracing::info_span!(
        "http.request",
        "http.method" = %method,
        "http.route" = path_label,
        "http.status_code" = tracing::field::Empty,
    );
    let span_for_record = span.clone();
    let response = next.run(request).instrument(span).await;
    let status = response.status().as_u16();
    span_for_record.record("http.status_code", status);
    state
        .metrics
        .http
        .observe(path_label, &method, status, start.elapsed().as_secs_f64());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_routes_map_to_themselves() {
        for r in [
            "/healthz",
            "/readyz",
            "/metrics",
            "/openapi.json",
            "/docs",
            "/api/config",
            "/api/status",
            "/api/sandboxes",
        ] {
            assert_eq!(route_label(r), r, "{r} should map to itself");
        }
    }

    #[test]
    fn parametric_sandbox_route_collapses_to_template() {
        assert_eq!(
            route_label("/api/sandboxes/owui-c-deadbeef"),
            "/api/sandboxes/{name}"
        );
        assert_eq!(
            route_label("/api/sandboxes/anything-with-!@#"),
            "/api/sandboxes/{name}"
        );
    }

    #[test]
    fn terminal_ws_collapses_to_fixed_label() {
        assert_eq!(route_label("/api/terminals/chat-42"), "terminal_ws");
    }

    #[test]
    fn catch_all_proxied_routes_collapse_to_proxy() {
        for r in [
            "/execute",
            "/files/list",
            "/files/read",
            "/snapshot",
            "/restore",
            "/api/terminals", // POST create → proxied (no trailing slash)
            "/download/foo/bar",
        ] {
            assert_eq!(route_label(r), "proxy", "{r} should be the proxy label");
        }
    }

    #[test]
    fn catalogue_registers_all_frozen_names() {
        let m = BrokerMetrics::new();
        let out = shared::gather(&m.registry);
        // The four non-HTTP collectors appear at zero immediately (single-child
        // gauges / counters are emitted on registration); the HTTP counter /
        // histogram families only materialise once observed, so the frozen-name
        // guard for those lives in the in-process app test (see `app.rs` tests).
        assert!(
            out.contains("open_websandbox_broker_active_sandboxes"),
            "{out}"
        );
        assert!(
            out.contains("open_websandbox_broker_sandboxes_created_total"),
            "{out}"
        );
        assert!(
            out.contains("open_websandbox_broker_sandboxes_deleted_total"),
            "{out}"
        );
        assert!(
            out.contains("open_websandbox_broker_runtime_hop_errors_total"),
            "{out}"
        );
        // HTTP metric families exist once observed.
        m.http.observe("/healthz", "GET", 200, 0.001);
        let out2 = shared::gather(&m.registry);
        assert!(
            out2.contains("open_websandbox_broker_http_requests_total"),
            "{out2}"
        );
        assert!(
            out2.contains("open_websandbox_broker_http_request_duration_seconds"),
            "{out2}"
        );
    }
}
