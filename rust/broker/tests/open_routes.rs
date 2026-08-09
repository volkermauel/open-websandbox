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
    assert!(body.contains("open-websandbox"), "{body}");
}

#[tokio::test]
async fn openapi_json_is_open_and_valid() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/openapi.json", Bearer::None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(body["openapi"], "3.0.3");
    assert!(body["paths"].is_object(), "{body}");
}

#[tokio::test]
async fn docs_serves_html() {
    let env = Env::new();
    let resp = env.send(Method::GET, "/docs", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("swagger-ui"), "{body}");
}
