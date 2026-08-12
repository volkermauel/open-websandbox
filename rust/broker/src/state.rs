//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use crate::metrics::BrokerMetrics;
use crate::s3::S3Offload;
use crate::store::SandboxStore;

/// State shared across all request handlers.
///
/// The [`shared::BrokerConfig`] is the env-driven drop-in config (D12); the
/// [`SandboxStore`] is the (real or stubbed) Kubernetes lifecycle backend,
/// type-erased so the HTTP handlers can be exercised in-process against an
/// in-memory store without a live cluster. The [`reqwest::Client`] is the shared
/// reverse-proxy upstream client (built once, reused per hop), and
/// [`Self::runtime_upstream_override`] is a test/dev seam that repoints the proxy
/// at a local mock server. [`Self::s3_restore`] is the optional C-4 cold-tier
/// driver wired in when `broker.s3.enabled` so resolve can restore a sandbox's
/// workspace on resume (it is `None` when the cold tier is off, in which case
/// resolve skips the restore hop).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<shared::BrokerConfig>,
    pub store: Arc<dyn SandboxStore>,
    pub http: reqwest::Client,
    pub runtime_upstream_override: Arc<Option<String>>,
    /// C-4 cold-tier restore driver. `Some` only when `broker.s3.enabled` (the
    /// same [`S3Offload`] the leader-gated reaper offloads through); `None`
    /// otherwise, in which case resolve never attempts an S3 restore.
    pub s3_restore: Option<Arc<S3Offload>>,
    /// D9 Prometheus metrics catalogue + registry (one per process; shared
    ///    by the request path and the leader-gated reaper).
    pub metrics: Arc<BrokerMetrics>,
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
            s3_restore: None,
            metrics: BrokerMetrics::new(),
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
            store: Arc::new(crate::store::test_fakes::StubSandboxStore::new()),
            http,
            runtime_upstream_override: Arc::new(None),
            s3_restore: None,
            metrics: BrokerMetrics::new(),
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

    /// Test seam: wire in a C-4 cold-tier restore driver (the same
    /// [`S3Offload`] the reaper offloads through) so resolve's restore-on-resume
    /// branch can be exercised in-process.
    #[must_use]
    pub fn with_s3_restore(mut self, restore: Arc<S3Offload>) -> Self {
        self.s3_restore = Some(restore);
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
