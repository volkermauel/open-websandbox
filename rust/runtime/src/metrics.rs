//! D9 — Prometheus metrics for the runtime: the frozen metric names, the HTTP
//! rate/latency pair (via [`shared::HttpMetrics`]) + execute-specific counters,
//! the templated route label normaliser, the axum HTTP middleware, and the
//! `/metrics` handler.
//!
//! ## Metric catalogue (frozen names)
//!
//! HTTP (one series per `{path, method, status}` — covers execute + files
//! rate / latency / errors, per issue #74 Q2):
//! - `open_websandbox_runtime_http_requests_total` (counter)
//! - `open_websandbox_runtime_http_request_duration_seconds` (histogram)
//!
//! Execute (the most security-sensitive surface):
//! - `open_websandbox_runtime_execute_commands_total` (counter)
//! - `open_websandbox_runtime_execute_timeouts_total` (counter)
//!
//! The `path` label is the **templated** matched route (bounded cardinality) —
//! see [`route_label`]. The runtime registers no catch-all, so an unmatched
//! request is reported as `path="unmatched"` (never the raw URL).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use shared::HttpMetrics;
use tracing::Instrument;

use crate::state::AppState;

// ---- Frozen metric names (Grafana dashboard contract; issue #74) ----
/// Frozen HTTP rate counter name.
pub(crate) const HTTP_REQUESTS_TOTAL: &str = "open_websandbox_runtime_http_requests_total";
/// Frozen HTTP latency histogram name.
pub(crate) const HTTP_REQUEST_DURATION: &str =
    "open_websandbox_runtime_http_request_duration_seconds";
/// Frozen execute-commands counter name.
pub(crate) const EXECUTE_COMMANDS_TOTAL: &str = "open_websandbox_runtime_execute_commands_total";
/// Frozen execute-timeouts counter name.
pub(crate) const EXECUTE_TIMEOUTS_TOTAL: &str = "open_websandbox_runtime_execute_timeouts_total";

/// Runtime HTTP rate/latency holder + the execute-specific counters + the
/// install point for the global recorder.
#[derive(Clone)]
pub struct RuntimeMetrics {
    pub http: HttpMetrics,
}

impl RuntimeMetrics {
    /// Install the global recorder, describe + seed the execute counters, and
    /// construct the HTTP rate/latency pair.
    #[must_use]
    pub fn new() -> Arc<Self> {
        // Install the single `metrics-exporter-prometheus` recorder for this
        // process (idempotent) before any describe/macro call.
        let _ = shared::install();

        // Describe + seed each execute counter so it materialises a live series
        // immediately (parity with the raw `prometheus` crate's eager register:
        // the facade only emits a series once a value is recorded).
        metrics::describe_counter!(
            EXECUTE_COMMANDS_TOTAL,
            "Commands executed via POST /execute (exits 124 on timeout, recorded \
             separately by execute_timeouts_total)."
        );
        metrics::counter!(EXECUTE_COMMANDS_TOTAL).increment(0);
        metrics::describe_counter!(
            EXECUTE_TIMEOUTS_TOTAL,
            "Commands killed by the /execute timeout (exit_code 124)."
        );
        metrics::counter!(EXECUTE_TIMEOUTS_TOTAL).increment(0);

        Arc::new(Self {
            http: HttpMetrics::new(HTTP_REQUESTS_TOTAL, HTTP_REQUEST_DURATION),
        })
    }
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        let arc = Self::new();
        Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone())
    }
}

/// Normalise a matched path into a **bounded** `path` label.
///
/// Static routes (no params) are reported verbatim — their `MatchedPath` equals
/// the template. Parametric routes (`/download/{*file_path}`, `/list/{*file_path}`,
/// `/exists/{*file_path}`, `/api/terminals/{id}`) are folded back to their
/// templates so arbitrary file paths / terminal ids never reach the label set.
#[must_use]
pub fn route_label(matched: &str) -> &str {
    match matched {
        "/" | "/healthz" | "/readyz" | "/metrics" | "/execute" | "/files/cwd" | "/files/list"
        | "/files/read" | "/files/write" | "/files/mkdir" | "/files/move" | "/files/delete"
        | "/files/view" | "/files/replace" | "/files/grep" | "/files/glob" | "/files/archive"
        | "/files/upload" | "/upload" | "/ports" | "/snapshot" | "/restore" | "/api/terminals"
        | "/list" | "/list/" => matched,
        _ if matched.starts_with("/download/") => "/download/{file_path}",
        _ if matched.starts_with("/list/") => "/list/{file_path}",
        _ if matched.starts_with("/exists/") => "/exists/{file_path}",
        _ if matched.starts_with("/api/terminals/") => "/api/terminals/{id}",
        // No catch-all on the runtime → an unmatched (404) request reports
        // `MatchedPath == None` upstream; this branch is a defensive fallback.
        _ => "unmatched",
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
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned());
    // The runtime has NO catch-all: a request that matched no route reports
    // `MatchedPath == None` → label it "unmatched" (never the raw URL, which
    // would be attacker-controlled and unbounded).
    let path_label = match matched.as_deref() {
        Some(m) => route_label(m),
        None => "unmatched",
    };
    // D9 — OTel: one span per served request (templated route + method + status).
    // When the OTLP bridge is active (OTEL_EXPORTER_OTLP_ENDPOINT set) this
    // becomes an exported span; otherwise it is a cheap no-op.
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

/// `GET /metrics` — Prometheus exposition (D9). Renders the runtime catalogue
/// (`open_websandbox_runtime_*`) in `text/plain; version=0.0.4`.
pub async fn metrics() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        shared::gather(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_routes_map_to_themselves() {
        for r in [
            "/",
            "/healthz",
            "/readyz",
            "/metrics",
            "/execute",
            "/files/read",
        ] {
            assert_eq!(route_label(r), r, "{r} should map to itself");
        }
    }

    #[test]
    fn file_path_routes_collapse_to_template() {
        assert_eq!(route_label("/download/a/b/c.txt"), "/download/{file_path}");
        assert_eq!(route_label("/list/sub/dir"), "/list/{file_path}");
        assert_eq!(route_label("/exists/missing"), "/exists/{file_path}");
    }

    #[test]
    fn terminal_id_collapses_to_template() {
        assert_eq!(route_label("/api/terminals/pty-7"), "/api/terminals/{id}");
    }

    #[test]
    fn unknown_route_falls_back_unmatched() {
        assert_eq!(route_label("/totally/made/up"), "unmatched");
    }

    #[test]
    fn catalogue_registers_all_frozen_names() {
        // Constructing the catalogue seeds each metric so the facade emits a
        // live series for every frozen name immediately (the global recorder is
        // shared across this test binary; presence — not value — is asserted).
        let m = RuntimeMetrics::new();
        // The HTTP rate/latency pair is a labelled vector — like the raw
        // `prometheus` crate it emits no child series until first observation,
        // so drive one templated observation to materialise it.
        m.http.observe("/execute", "POST", 200, 0.5);
        let out = shared::gather();
        for name in [
            HTTP_REQUESTS_TOTAL,
            HTTP_REQUEST_DURATION,
            EXECUTE_COMMANDS_TOTAL,
            EXECUTE_TIMEOUTS_TOTAL,
        ] {
            assert!(out.contains(name), "missing frozen metric {name}:\n{out}");
        }
    }
}
