//! PR-C-2 integration: `resolve_sandbox` (get-or-create + ready-poll) and the
//! catch-all reverse proxy, exercised in-process against the `StubSandboxStore`
//! and a `wiremock` upstream — no live Kubernetes cluster required.

#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{build_router, resolve_sandbox, sandbox_name, ApiError, AppState, StubSandboxStore};
use shared::{
    BrokerConfig, Profile, Sandbox, SandboxCondition, SandboxSpec, SandboxStatus,
    SandboxTemplate, SandboxTemplateSpec,
};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const STRONG_SECRET: &str = "test-broker-shared-secret-0123456789";
const RT_KEY: &str = "rt-test-key";
const BASE_TEMPLATE: &str = "code-standard-v1";

fn template() -> SandboxTemplate {
    SandboxTemplate::new(
        BASE_TEMPLATE,
        SandboxTemplateSpec {
            description: None,
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "code-standard:latest",
                        "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]
                    }],
                    "volumes": [{"name": "workspace", "emptyDir": {}}]
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

fn store_with_template() -> Arc<StubSandboxStore> {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template());
    store
}

fn config(timeout: u64) -> BrokerConfig {
    BrokerConfig {
        shared_secret: STRONG_SECRET.into(),
        runtime_ns: "agent-sandbox-runtime".into(),
        base_template: BASE_TEMPLATE.into(),
        runtime_api_key: RT_KEY.into(),
        claim_timeout_seconds: timeout,
        ..BrokerConfig::default()
    }
}

fn state(store: Arc<StubSandboxStore>, cfg: BrokerConfig) -> AppState {
    AppState::new(cfg, store)
}

fn authed_req(method: &str, uri: &str, body: &'static [u8]) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {STRONG_SECRET}"))
        .header("x-user-id", "user-1")
        .header("x-session-id", "chat-1")
        .header("x-persistence", "persistent")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request")
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).unwrap_or_default()
}

// --- resolve_sandbox --------------------------------------------------------

#[tokio::test]
async fn resolve_returns_existing_ready_sandbox_without_creating() {
    let store = store_with_template();
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut existing = Sandbox::new(&name, SandboxSpec::default());
    existing.metadata.namespace = Some("agent-sandbox-runtime".into());
    existing.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(existing);

    let state = state(store.clone(), config(5));
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("ready");
    assert_eq!(resolved.name, name);
    assert_eq!(resolved.pod_ip, "10.0.0.9");
    // No new sandbox created.
    assert_eq!(store.snapshot().len(), 1);
}

#[tokio::test]
async fn resolve_creates_and_waits_for_ready_when_absent() {
    let store = store_with_template();
    // Simulate the controller flipping a freshly-created sandbox to Ready.
    store.set_auto_ready_on_create(Some("10.0.0.5".into()));

    let state = state(store.clone(), config(5));
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("created + ready");

    let expected_name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    assert_eq!(resolved.name, expected_name);
    assert_eq!(resolved.pod_ip, "10.0.0.5");
    // The sandbox was created from the base template (managed-by + profile label).
    let snap = store.snapshot();
    let created = snap.get(&expected_name).expect("sandbox created");
    let labels = created.metadata.labels.as_ref().expect("labels");
    assert_eq!(
        labels.get("app.kubernetes.io/managed-by").unwrap(),
        "owui-broker"
    );
    assert_eq!(labels.get("broker-profile").unwrap(), "persistent");
}

#[tokio::test]
async fn resolve_deterministic_name_matches_python_scheme() {
    let store = store_with_template();
    store.set_auto_ready_on_create(Some("10.0.0.2".into()));
    let state = state(store.clone(), config(5));

    let e = resolve_sandbox(&state, "u-1", "s-1", Profile::Ephemeral)
        .await
        .unwrap();
    // Python: owui- + sha256("u-1|s-1")[:12].
    assert!(e.name.starts_with("owui-"), "{}", e.name);

    let p = resolve_sandbox(&state, "u-1", "s-1", Profile::Persistent)
        .await
        .unwrap();
    // Python: owui-c- + sha256("u-1/s-1")[:12].
    assert!(p.name.starts_with("owui-c-"), "{}", p.name);
}

#[tokio::test]
async fn resolve_times_out_when_never_ready() {
    let store = store_with_template();
    // auto_ready disabled ⇒ created sandbox never reaches Ready.
    let state = state(store.clone(), config(0));
    let err = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect_err("must time out");
    assert!(matches!(err, ApiError::ServiceUnavailable(_)), "{err:?}");
}

// --- end-to-end reverse proxy ----------------------------------------------

async fn proxy_env(upstream: &str, store: Arc<StubSandboxStore>) -> AppState {
    store.set_auto_ready_on_create(Some("10.0.0.7".into()));
    let cfg = BrokerConfig {
        shared_secret: STRONG_SECRET.into(),
        runtime_ns: "agent-sandbox-runtime".into(),
        base_template: BASE_TEMPLATE.into(),
        runtime_api_key: RT_KEY.into(),
        claim_timeout_seconds: 5,
        proxy_timeout_seconds: 10,
        ..BrokerConfig::default()
    };
    let state = AppState::new(cfg, store);
    state.with_runtime_upstream_override(upstream.to_string())
}

#[tokio::test]
async fn proxy_forwards_method_path_body_and_injects_runtime_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .and(header("authorization", format!("Bearer {RT_KEY}")))
        .and(header(
            "x-sandbox-id",
            sandbox_name("user-1", "chat-1", Profile::Persistent).as_str(),
        ))
        .and(header("x-sandbox-namespace", "agent-sandbox-runtime"))
        .and(header("x-sandbox-pod-ip", "10.0.0.7"))
        .and(header("x-session-id", "chat-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("command ran"))
        .mount(&server)
        .await;

    let store = store_with_template();
    // PR-C-5 / #4: seed the per-session runtime key so the proxy injects RT_KEY
    // (otherwise ensure_runtime_key mints a random one the mock wouldn't match).
    store.set_runtime_key(&sandbox_name("user-1", "chat-1", Profile::Persistent), RT_KEY);
    let state = proxy_env(&server.uri(), store).await;
    let app = build_router(state);
    let resp = app
        .oneshot(authed_req("POST", "/execute", br#"{"command":"ls"}"#))
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, "command ran");

    // The upstream received the client's body verbatim.
    let received = server.received_requests().await.expect("captured");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].method.as_str(), "POST");
    assert_eq!(received[0].url.path(), "/execute");
    assert_eq!(&received[0].body, br#"{"command":"ls"}"#);
}

#[tokio::test]
async fn proxy_forwards_query_string_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let state = proxy_env(&server.uri(), store_with_template()).await;
    let app = build_router(state);
    let resp = app
        .oneshot(authed_req("GET", "/files/list?recursive=1", b""))
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::OK);

    let received = server.received_requests().await.expect("captured");
    assert_eq!(received[0].url.query(), Some("recursive=1"));
}

#[tokio::test]
async fn proxy_rewrites_3xx_location_to_be_broker_relative() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/list"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", "http://10.0.0.7:8888/files/list/."),
        )
        .mount(&server)
        .await;

    let state = proxy_env(&server.uri(), store_with_template()).await;
    let app = build_router(state);
    let resp = app
        .oneshot(authed_req("GET", "/files/list", b""))
        .await
        .expect("router");
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "/files/list/.",
        "redirect Location must drop the unreachable pod IP"
    );
}

#[tokio::test]
async fn proxy_requires_x_user_id() {
    let server = MockServer::start().await;
    let state = proxy_env(&server.uri(), store_with_template()).await;
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/execute")
        .header("authorization", format!("Bearer {STRONG_SECRET}"))
        // X-User-Id deliberately omitted.
        .body(Body::from(crate_body()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn proxy_requires_bearer_auth() {
    let server = MockServer::start().await;
    let state = proxy_env(&server.uri(), store_with_template()).await;
    let app = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/execute")
        .header("x-user-id", "user-1")
        .body(Body::from(crate_body()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn crate_body() -> Vec<u8> {
    b"{}".to_vec()
}
