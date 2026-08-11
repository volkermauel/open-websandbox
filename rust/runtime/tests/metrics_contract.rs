//! D9 — Prometheus `/metrics` contract test (runtime).
//!
//! Drives the real in-process router (with the metrics middleware) and asserts
//! the frozen `open_websandbox_runtime_*` metric names + the templated `path`
//! label appear in `/metrics` exposition. Mirrors the broker-side guard in
//! `broker/tests/open_routes.rs::metrics_is_open_text`.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{body_text, status, Bearer, Env};

/// `/metrics` is open (no auth) and exposes the full frozen runtime catalogue,
/// including a templated `path` label for a previously-served open route.
#[tokio::test]
async fn metrics_exposes_frozen_catalogue_and_path_label() {
    let env = Env::new();

    // Serve an open route so the HTTP counter / histogram series for the
    // templated route materialise (a freshly-registered Vec emits no child
    // until the first observation).
    let probe = env
        .send(Method::GET, "/healthz", Bearer::None, None, None)
        .await;
    assert_eq!(status(&probe), StatusCode::OK);
    let _ = body_text(probe).await;

    let resp = env
        .send(Method::GET, "/metrics", Bearer::None, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain; version=0.0.4"
    );
    let body = body_text(resp).await;

    for name in [
        "open_websandbox_runtime_http_requests_total",
        "open_websandbox_runtime_http_request_duration_seconds",
        "open_websandbox_runtime_execute_commands_total",
        "open_websandbox_runtime_execute_timeouts_total",
    ] {
        assert!(body.contains(name), "missing frozen metric {name}:\n{body}");
    }

    // The `path` label carries the templated matched route (bounded cardinality),
    // not the raw URL — the /healthz probe shows up under path="/healthz".
    assert!(
        body.contains(r#"path="/healthz""#),
        "expected templated path label for /healthz:\n{body}"
    );
}
