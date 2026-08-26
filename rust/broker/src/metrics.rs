//! D9 — Prometheus metrics for the broker: the frozen metric names, the
//! templated-route label normaliser, and the axum HTTP middleware that records
//! rate / latency through the `metrics` facade (issue #74 Q4).
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
//! - `open_websandbox_broker_auth_failures_total` (counter, `outcome` label; #99 A6)
//! - `open_websandbox_broker_idle_reaps_total` (counter, `reason` label; #99 A6)
//!
//! HTTP rate/latency is recorded via [`shared::HttpMetrics`]; the lifecycle
//! counters/gauge are recorded with `metrics::counter!` / `metrics::gauge!` at
//! their call sites (api / reaper / resolve / proxy). The `path` label is the
//! **templated** matched route (bounded cardinality) — see [`route_label`].

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use shared::HttpMetrics;
use tracing::Instrument;

use crate::state::AppState;

// ---- Frozen metric names (Grafana dashboard contract; issue #74) ----
/// Frozen HTTP rate counter name.
pub(crate) const HTTP_REQUESTS_TOTAL: &str = "open_websandbox_broker_http_requests_total";
/// Frozen HTTP latency histogram name.
pub(crate) const HTTP_REQUEST_DURATION: &str =
    "open_websandbox_broker_http_request_duration_seconds";
/// Frozen active-sandbox gauge name.
pub(crate) const ACTIVE_SANDBOXES: &str = "open_websandbox_broker_active_sandboxes";
/// Frozen sandbox-create counter name.
pub(crate) const SANDBOXES_CREATED_TOTAL: &str = "open_websandbox_broker_sandboxes_created_total";
/// Frozen sandbox-delete counter name.
pub(crate) const SANDBOXES_DELETED_TOTAL: &str = "open_websandbox_broker_sandboxes_deleted_total";
/// Frozen runtime-hop-error counter name.
pub(crate) const RUNTIME_HOP_ERRORS_TOTAL: &str = "open_websandbox_broker_runtime_hop_errors_total";
/// Frozen auth-failure counter name (shared-Bearer guard + WS first-message auth).
pub(crate) const AUTH_FAILURES_TOTAL: &str = "open_websandbox_broker_auth_failures_total";
/// Frozen idle-reap counter name (leader reaper deletes, by reason).
pub(crate) const IDLE_REAPS_TOTAL: &str = "open_websandbox_broker_idle_reaps_total";

/// Broker HTTP rate/latency holder + the install point for the global
/// recorder.
///
/// One instance lives on [`AppState`] so a single scrape gathers the full
/// catalogue. The lifecycle counters/gauge are recorded through the facade
/// directly at their call sites; this holder owns only the HTTP pair (whose
/// templated labels + buckets are shared with the runtime).
#[derive(Clone)]
pub struct BrokerMetrics {
    /// HTTP request rate/latency pair (templated labels + buckets shared with the runtime).
    pub http: HttpMetrics,
}

impl BrokerMetrics {
    /// Install the global recorder, describe + seed the lifecycle metrics, and
    /// construct the HTTP rate/latency pair.
    #[must_use]
    pub fn new() -> Arc<Self> {
        // Install the single `metrics-exporter-prometheus` recorder for this
        // process (idempotent) before any describe/macro call.
        let _ = shared::install();

        // Describe + seed each lifecycle metric so it materialises a live
        // series immediately (parity with the raw `prometheus` crate's eager
        // register: the facade only emits a series once a value is recorded).
        metrics::describe_gauge!(
            ACTIVE_SANDBOXES,
            "Broker-owned sandboxes currently observed by the elected leader \
             (updated each leader reaper tick)."
        );
        metrics::gauge!(ACTIVE_SANDBOXES).set(0.0);
        metrics::describe_counter!(
            SANDBOXES_CREATED_TOTAL,
            "Sandboxes created via the broker (resolve get-or-create + explicit \
             POST /api/sandboxes)."
        );
        metrics::counter!(SANDBOXES_CREATED_TOTAL).increment(0);
        metrics::describe_counter!(
            SANDBOXES_DELETED_TOTAL,
            "Sandboxes deleted via the broker (reaper reap + explicit DELETE \
             /api/sandboxes/{name})."
        );
        metrics::counter!(SANDBOXES_DELETED_TOTAL).increment(0);
        metrics::describe_counter!(
            RUNTIME_HOP_ERRORS_TOTAL,
            "Reverse-proxy hops to a resolved runtime pod that failed \
             (transport / connect / send errors)."
        );
        metrics::counter!(RUNTIME_HOP_ERRORS_TOTAL).increment(0);
        metrics::describe_counter!(
            AUTH_FAILURES_TOTAL,
            "Broker auth-guard rejections by outcome (HTTP shared-Bearer `Authed` \
             extractor + WS terminal first-message auth)."
        );
        // Seed every frozen outcome so the labelled vector materialises a live
        // series immediately (dashboards see all outcomes before the first failure).
        for outcome in [
            "missing_token",
            "bad_token",
            "misconfigured_secret",
            "auth_timeout",
        ] {
            metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => outcome).increment(0);
        }
        metrics::describe_counter!(
            IDLE_REAPS_TOTAL,
            "Sandboxes reaped (deleted) by the leader idle reaper, by reason."
        );
        for reason in ["ephemeral_idle", "s3_tiered_idle", "persistent_reap_ttl"] {
            metrics::counter!(IDLE_REAPS_TOTAL, "reason" => reason).increment(0);
        }

        Arc::new(Self {
            http: HttpMetrics::new(HTTP_REQUESTS_TOTAL, HTTP_REQUEST_DURATION),
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
        // Constructing the catalogue seeds each metric so the facade emits a
        // live series for every frozen name immediately (the global recorder
        // is shared across this test binary; presence — not value — is what we
        // assert here).
        let m = BrokerMetrics::new();
        // The HTTP rate/latency pair is a labelled vector — like the raw
        // `prometheus` crate it emits no child series until first observation,
        // so drive one templated observation to materialise it.
        m.http.observe("/healthz", "GET", 200, 0.001);
        let out = shared::gather();
        for name in [
            HTTP_REQUESTS_TOTAL,
            HTTP_REQUEST_DURATION,
            ACTIVE_SANDBOXES,
            SANDBOXES_CREATED_TOTAL,
            SANDBOXES_DELETED_TOTAL,
            RUNTIME_HOP_ERRORS_TOTAL,
            AUTH_FAILURES_TOTAL,
            IDLE_REAPS_TOTAL,
        ] {
            assert!(out.contains(name), "missing frozen metric {name}:\n{out}");
        }
    }
}
