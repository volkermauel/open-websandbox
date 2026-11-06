//! `GET /api/config` runtime-forwarding contract (desktop-sidecar lane).
//!
//! The broker's `/api/config` serves the RUNTIME's merged config (runtime
//! flags + desktop/browser/git/events from the sidecar) through the same
//! resolve+auth path as the catch-all proxy, degrading to the broker's local
//! flags — never a 5xx — whenever that path fails. Exercised in-process
//! against the `StubSandboxStore` and a `wiremock` stub runtime.

#![forbid(unsafe_code)]
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::test_fakes::StubSandboxStore;
use broker::{build_router, sandbox_name, AppState, SandboxStore};
use shared::{
    BrokerConfig, Profile, Sandbox, SandboxCondition, SandboxSpec, SandboxStatus, SandboxTemplate,
    SandboxTemplateSpec,
};
use tower::util::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const STRONG_SECRET: &str = "test-broker-shared-secret-0123456789";
const BASE_TEMPLATE: &str = "code-standard-v1";

/// The broker's previous (pre-desktop) `/api/config` body, verbatim.
const LOCAL_FLAGS: &str = r#"{"features":{"terminal":true,"notebooks":false,"system":true}}"#;

fn template() -> SandboxTemplate {
    SandboxTemplate::new(
        BASE_TEMPLATE,
        SandboxTemplateSpec {
            description: None,
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "code-standard:latest"
                    }]
                }
            })),
        },
    )
}

fn ready_status(ip: &str) -> SandboxStatus {
    SandboxStatus {
        phase: Some("Running".into()),
        pod_i_ps: Some(vec![ip.into()]),
        conditions: Some(vec![SandboxCondition {
            r#type: "Ready".into(),
            status: "True".into(),
            reason: None,
            message: None,
            last_transition_time: None,
        }]),
        ready: Some(true),
        message: None,
    }
}

/// Env with a seeded Ready sandbox (user-1/chat-1/persistent) whose runtime
/// key is pre-minted, pointed at `upstream` via the proxy override seam.
async fn env_at(upstream: &str) -> (AppState, Arc<StubSandboxStore>, String) {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template());
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut existing = Sandbox::new(&name, SandboxSpec::default());
    existing.metadata.namespace = Some("agent-sandbox-runtime".into());
    existing.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(existing);
    store
        .ensure_runtime_key(&name)
        .await
        .expect("mint runtime key");
    let key = store
        .read_runtime_key(&name)
        .await
        .expect("read key")
        .expect("key minted");

    let cfg = BrokerConfig {
        shared_secret: STRONG_SECRET.into(),
        runtime_ns: "agent-sandbox-runtime".into(),
        base_template: BASE_TEMPLATE.into(),
        claim_timeout_seconds: 5,
        ..BrokerConfig::default()
    };
    let state = AppState::new(cfg, store.clone());
    (
        state.with_runtime_upstream_override(upstream.to_string()),
        store,
        key,
    )
}

fn config_req(identity: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/config")
        .header("authorization", format!("Bearer {STRONG_SECRET}"));
    if identity {
        builder = builder
            .header("x-user-id", "user-1")
            .header("x-session-id", "chat-1")
            .header("x-persistence", "persistent");
    }
    builder.body(Body::empty()).expect("request")
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.expect("body");
    String::from_utf8(bytes.to_vec()).unwrap_or_default()
}

/// A dead (refused) loopback port for the unreachable-runtime cases.
fn refused_port() -> u16 {
    let l = StdTcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}

#[tokio::test]
async fn config_serves_runtime_merged_flags_through_the_resolve_auth_path() {
    let server = MockServer::start().await;
    // Header matchers prove the request took the SAME resolve+auth path as the
    // catch-all: runtime-key Bearer + sandbox identity, on GET /api/config.
    let (state, _store, key) = env_at(&server.uri()).await;
    Mock::given(method("GET"))
        .and(path("/api/config"))
        .and(header("authorization", format!("Bearer {key}")))
        .and(header(
            "x-sandbox-id",
            sandbox_name("user-1", "chat-1", Profile::Persistent).as_str(),
        ))
        .and(header("x-sandbox-namespace", "agent-sandbox-runtime"))
        .and(header("x-sandbox-pod-ip", "10.0.0.9"))
        .and(header("x-session-id", "chat-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "features": {
                "terminal": true,
                "notebooks": false,
                "system": true,
                "desktop": true,
                "browser": true,
                "git": true,
                "events": true,
            }
        })))
        .mount(&server)
        .await;

    let resp = build_router(state)
        .oneshot(config_req(true))
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    let cfg: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    let features = &cfg["features"];
    assert_eq!(features["desktop"], true, "desktop flag forwarded: {body}");
    assert_eq!(features["browser"], true, "browser flag forwarded: {body}");
    assert_eq!(features["git"], true, "git flag forwarded: {body}");
    assert_eq!(features["events"], true, "events flag forwarded: {body}");
    assert_eq!(features["terminal"], true);
    assert_eq!(features["notebooks"], false);
    assert_eq!(features["system"], true);
    // Exactly one runtime hop — the matched stub (unmatched requests would
    // have degraded to the local flags and failed the assertions above).
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn config_degrades_to_local_flags_when_runtime_errors() {
    let server = MockServer::start().await;
    let (state, _store, _key) = env_at(&server.uri()).await;
    Mock::given(method("GET"))
        .and(path("/api/config"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let resp = build_router(state)
        .oneshot(config_req(true))
        .await
        .expect("router");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "runtime 5xx must not propagate"
    );
    assert_eq!(body_string(resp).await, LOCAL_FLAGS);
}

#[tokio::test]
async fn config_degrades_to_local_flags_when_runtime_is_unreachable() {
    let dead = format!("http://127.0.0.1:{}", refused_port());
    let (state, _store, _key) = env_at(&dead).await;

    let resp = build_router(state)
        .oneshot(config_req(true))
        .await
        .expect("router");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "unreachable runtime must not 5xx"
    );
    assert_eq!(body_string(resp).await, LOCAL_FLAGS);
}

#[tokio::test]
async fn config_without_identity_serves_local_flags_and_claims_nothing() {
    let server = MockServer::start().await;
    let (state, store, _key) = env_at(&server.uri()).await;
    Mock::given(method("GET"))
        .and(path("/api/config"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must-not-reach"))
        .mount(&server)
        .await;

    // OWUI's pre-session config probe carries no X-User-Id: degrade to the
    // local flags WITHOUT resolving (i.e. never creating) a sandbox.
    let resp = build_router(state)
        .oneshot(config_req(false))
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, LOCAL_FLAGS);
    // Only the seeded sandbox exists — no claim side effect.
    assert_eq!(store.snapshot().len(), 1);
    assert!(server.received_requests().await.unwrap().is_empty());
}
