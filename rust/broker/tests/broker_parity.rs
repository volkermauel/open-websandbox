//! Broker-served OWUI parity routes (`/api/config`, `/api/status`) and the
//! catch-all reverse-proxy auth gate (forwarding is covered in `proxy_resolve.rs`).

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{body_text, json, status, Bearer, Env};

#[tokio::test]
async fn api_config_matches_openapi_spec_shape() {
    // ConfigResponse: {"features":{"terminal":true,"notebooks":false,"system":false}} (open-terminal v0.12.3 key set)
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::Default, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(
        body,
        serde_json::json!({"features": {"terminal": true, "notebooks": false, "system": false}})
    );
}

#[tokio::test]
async fn api_config_feature_keys_in_canonical_order() {
    // Field order matters for byte parity (D11). serde serializes struct fields
    // in declaration order, so the JSON text must read terminal, notebooks, system.
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::Default, None)
        .await;
    let text = body_text(resp).await;
    let a = text.find(r#""terminal""#).unwrap();
    let b = text.find(r#""notebooks""#).unwrap();
    let c = text.find(r#""system""#).unwrap();
    assert!(a < b && b < c, "feature key order wrong: {text}");
}

#[tokio::test]
async fn api_status_matches_openapi_spec_shape() {
    // StatusResponse: {"active_pods":0,"max_pods":10,"pods":[]}
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/status", Bearer::Default, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let body: serde_json::Value = json(resp).await;
    assert_eq!(body["active_pods"], 0);
    assert_eq!(body["max_pods"], 10);
    assert!(body["pods"].is_array());
    assert!(body["pods"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn api_config_and_status_require_auth() {
    let env = Env::new();
    assert_eq!(
        status(
            &env.send(Method::GET, "/api/config", Bearer::None, None)
                .await
        ),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &env.send(Method::GET, "/api/status", Bearer::None, None)
                .await
        ),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn catch_all_proxy_requires_auth() {
    let env = Env::new();
    let resp = env.send(Method::POST, "/execute", Bearer::None, None).await;
    assert_eq!(status(&resp), StatusCode::UNAUTHORIZED);
}
