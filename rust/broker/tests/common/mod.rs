//! Shared harness for the broker integration tests.
//!
//! Builds an in-process axum router over a configured [`BrokerConfig`] + an
//! in-memory [`StubSandboxStore`], and drives it via `tower::ServiceExt::oneshot`
//! so the tests exercise the real HTTP extractors/handlers without a network
//! server or a live apiserver.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Method, Request, Response, StatusCode};
use broker::{build_router, AppState, StubSandboxStore};
use shared::{BrokerConfig, Profile, Sandbox, SandboxTemplate, SandboxTemplateSpec};
use tower::util::ServiceExt;

pub const STRONG_SECRET: &str = "a-very-strong-and-random-broker-shared-secret";

/// A reusable base template name + a pod blueprint for seeding the stub store.
pub const BASE_TEMPLATE: &str = "code-standard-v1";

/// One broker harness: config + in-memory store + built router.
pub struct Env {
    pub secret: String,
    pub store: Arc<StubSandboxStore>,
    router: axum::Router,
}

impl Env {
    /// Default env: strong shared secret, reachable apiserver, base template seeded.
    pub fn new() -> Self {
        Self::build(STRONG_SECRET.to_string(), true)
    }

    /// Env with a custom shared secret (e.g. a placeholder to exercise fail-closed).
    pub fn with_secret(secret: impl Into<String>) -> Self {
        Self::build(secret.into(), true)
    }

    /// Env with an explicit apiserver-reachable flag (drives `/readyz`).
    pub fn with_reachable(reachable: bool) -> Self {
        Self::build(STRONG_SECRET.to_string(), reachable)
    }

    fn build(secret: String, reachable: bool) -> Self {
        let config = BrokerConfig {
            shared_secret: secret.clone(),
            ..broker_test_config()
        };
        let store = Arc::new(StubSandboxStore::new());
        store.set_reachable(reachable);
        store.insert_template(seed_template(BASE_TEMPLATE));
        let state = AppState::new(config, store.clone());
        let router = build_router(state);
        Self {
            secret,
            store,
            router,
        }
    }

    /// Mark the apiserver unreachable (e.g. to assert `/readyz` flips to 503).
    pub fn set_reachable(&self, reachable: bool) {
        self.store.set_reachable(reachable);
    }

    /// Send a request with optional bearer control + optional JSON body.
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        bearer: Bearer<'_>,
        body: Option<String>,
    ) -> Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        match bearer {
            Bearer::Default => {
                builder = builder.header("authorization", format!("Bearer {}", self.secret));
            }
            Bearer::Explicit(v) => {
                builder = builder.header("authorization", format!("Bearer {v}"));
            }
            Bearer::None => {}
        }
        let req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        self.router.clone().oneshot(req).await.expect("oneshot")
    }
}

/// Minimal-but-real config for tests (strong secret unless overridden).
fn broker_test_config() -> BrokerConfig {
    BrokerConfig {
        max_terminal_sessions: 8,
        max_output_bytes: 1_048_576,
        shared_secret: String::new(),
        runtime_ns: "agent-sandbox-runtime".to_string(),
        base_template: BASE_TEMPLATE.to_string(),
        default_profile: Profile::Persistent,
        runtime_api_key: "rt-test-key".to_string(),
        claim_timeout_seconds: 60,
        proxy_timeout_seconds: 660,
        ..Default::default()
    }
}

/// A base `SandboxTemplate` with an emptyDir-backed workspace pod blueprint.
pub fn seed_template(name: &str) -> SandboxTemplate {
    SandboxTemplate::new(
        name,
        SandboxTemplateSpec {
            description: Some("test template".into()),
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "code-standard:latest"
                    }],
                    "volumes": [{"name": "workspace", "emptyDir": {}}]
                }
            })),
        },
    )
}

/// Bearer control for [`Env::send`].
pub enum Bearer<'a> {
    /// Inject the default (correct) shared secret.
    Default,
    /// Inject an explicit token.
    Explicit(&'a str),
    /// No Authorization header.
    None,
}

/// Read a response body to a `String`.
pub async fn body_text(resp: Response<Body>) -> String {
    String::from_utf8_lossy(&body_bytes(resp).await).into_owned()
}

/// Read a response body to raw bytes.
pub async fn body_bytes(resp: Response<Body>) -> Bytes {
    to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .expect("collect body")
}

/// Status of a response.
pub fn status(resp: &Response<Body>) -> StatusCode {
    resp.status()
}

/// Parse a JSON response body into `T`.
pub async fn json<T: serde::de::DeserializeOwned>(resp: Response<Body>) -> T {
    let text = body_text(resp).await;
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("json decode failed: {e}\n{text}"))
}

/// Insert a sandbox into the stub store (convenience for get/list seeding).
#[allow(dead_code)]
pub fn insert_sandbox(env: &Env, sandbox: Sandbox) {
    env.store.insert_sandbox(sandbox);
}
