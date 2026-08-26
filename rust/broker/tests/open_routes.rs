//! Open (unauthenticated) broker routes: healthz, readyz, metrics, openapi.json, docs.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{body_text, json, status, Bearer, Env};

#[tokio::test]
async fn healthz_is_open_and_returns_ok() {
    let env = Env::new();
    let resp = env.send(Method::GET, "/healthz", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(body, serde_json::json!({"status": "ok"}));
}

#[tokio::test]
async fn readyz_is_ready_when_apiserver_reachable() {
    let env = Env::with_reachable(true);
    let resp = env.send(Method::GET, "/readyz", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(body, serde_json::json!({"status": "ready"}));
}

#[tokio::test]
async fn readyz_is_503_when_apiserver_unreachable() {
    let env = Env::with_reachable(false);
    let resp = env.send(Method::GET, "/readyz", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(body["detail"], "apiserver unreachable");
}

#[tokio::test]
async fn metrics_is_open_text() {
    let env = Env::new();
    // Hit an open route first so the HTTP counter / histogram series for the
    // templated route materialises (a freshly-registered Vec emits no child
    // until first observation).
    let probe = env.send(Method::GET, "/healthz", Bearer::None, None).await;
    assert_eq!(status(&probe), StatusCode::OK);
    let _ = body_text(probe).await;

    let resp = env.send(Method::GET, "/metrics", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let body = body_text(resp).await;
    // D9: the stub is gone — the frozen broker metric catalogue is present.
    for name in [
        "open_websandbox_broker_http_requests_total",
        "open_websandbox_broker_http_request_duration_seconds",
        "open_websandbox_broker_active_sandboxes",
        "open_websandbox_broker_sandboxes_created_total",
        "open_websandbox_broker_sandboxes_deleted_total",
        "open_websandbox_broker_runtime_hop_errors_total",
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

#[tokio::test]
async fn openapi_json_is_open_and_valid() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/openapi.json", Bearer::None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    // utoipa 5 emits OpenAPI 3.1.0 (D10).
    assert_eq!(body["openapi"], "3.1.0");
    assert!(body["paths"].is_object(), "{body}");
}

#[tokio::test]
async fn docs_serves_html() {
    let env = Env::new();
    let resp = env.send(Method::GET, "/docs", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body = body_text(resp).await;
    // /docs serves Scalar (issue #75 Q3): the HTML inlines the spec under the
    // `api-reference` script id.
    assert!(body.contains("api-reference"), "{body}");
}
