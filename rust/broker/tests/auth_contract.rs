//! Shared-Bearer auth contract: gated routes are fail-closed.
//!
//! Mirrors the Python broker's `_auth`: placeholder/unset secret → 503;
//! missing or mismatched Bearer → 401; correct Bearer → 200.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{json, status, Bearer, Env};
use shared::is_placeholder_secret;

#[tokio::test]
async fn gated_route_requires_bearer() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json::<serde_json::Value>(resp).await["detail"],
        "invalid bearer token"
    );
}

#[tokio::test]
async fn gated_route_rejects_wrong_bearer() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::Explicit("nope"), None)
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gated_route_accepts_correct_bearer() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::Default, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
}

#[tokio::test]
async fn placeholder_secret_fails_closed_with_503() {
    // Any placeholder from the shared set must be treated as "not configured".
    for placeholder in shared::PLACEHOLDER_SECRETS {
        assert!(is_placeholder_secret(placeholder));
        let env = Env::with_secret(*placeholder);
        let resp = env
            .send(
                Method::GET,
                "/api/config",
                Bearer::Explicit("anything"),
                None,
            )
            .await;
        assert_eq!(
            status(&resp),
            StatusCode::SERVICE_UNAVAILABLE,
            "placeholder {placeholder:?} should be 503"
        );
        assert_eq!(
            json::<serde_json::Value>(resp).await["detail"],
            "BROKER_SHARED_SECRET is not configured"
        );
    }
}

#[tokio::test]
async fn open_routes_work_without_bearer() {
    let env = Env::new();
    let resp = env.send(Method::GET, "/healthz", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::OK);
}

#[tokio::test]
async fn sandbox_crud_requires_bearer() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/sandboxes", Bearer::None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}
