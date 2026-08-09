//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use crate::store::SandboxStore;

/// State shared across all request handlers.
///
/// The [`shared::BrokerConfig`] is the env-driven drop-in config (D12); the
/// [`SandboxStore`] is the (real or stubbed) Kubernetes lifecycle backend,
/// type-erased so the HTTP handlers can be exercised in-process against an
/// in-memory store without a live cluster. The [`reqwest::Client`] is the shared
/// reverse-proxy upstream client (built once, reused per hop), and
/// [`Self::runtime_upstream_override`] is a test/dev seam that repoints the proxy
/// at a local mock server.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<shared::BrokerConfig>,
    pub store: Arc<dyn SandboxStore>,
    pub http: reqwest::Client,
    pub runtime_upstream_override: Arc<Option<String>>,
}

impl AppState {
    /// Build state from a config and a store backend. Constructs the shared
    /// reqwest client (rustls, no redirects — the proxy rewrites `Location`
    /// itself) bounded by the configured proxy timeout.
    pub fn new(config: shared::BrokerConfig, store: Arc<dyn SandboxStore>) -> Self {
        let http = http_client(config.proxy_timeout_seconds);
        Self {
            config: Arc::new(config),
            store,
            http,
            runtime_upstream_override: Arc::new(None),
        }
    }

    /// Test seam: a store-backed-but-defaulty state for unit-testing pure proxy
    /// logic that never touches the store (e.g. URL construction).
    #[cfg(test)]
    #[must_use]
    pub fn for_test(config: shared::BrokerConfig) -> Self {
        let http = http_client(config.proxy_timeout_seconds);
        Self {
            config: Arc::new(config),
            store: Arc::new(crate::store::StubSandboxStore::new()),
            http,
            runtime_upstream_override: Arc::new(None),
        }
    }

    /// Test/dev seam: force the reverse-proxy upstream base URL (e.g.
    /// `http://127.0.0.1:<port>` of a local mock server) instead of the real
    /// `http://<pod-ip>:8888`. `None` in production.
    #[must_use]
    pub fn with_runtime_upstream_override(mut self, base: impl Into<String>) -> Self {
        self.runtime_upstream_override = Arc::new(Some(base.into()));
        self
    }
}

/// Build the shared reverse-proxy reqwest client: rustls, no redirects,
/// total-request timeout = the configured proxy timeout.
fn http_client(proxy_timeout_seconds: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(proxy_timeout_seconds))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builder must not fail with valid settings")
}
