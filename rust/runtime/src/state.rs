//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::auth::SessionKeyStore;
use crate::config::RuntimeConfig;
use crate::desktop::DesktopConfigCache;
use crate::metrics::RuntimeMetrics;
use crate::terminals::TerminalRegistry;

/// State shared across all request handlers.
///
/// Cheaply cloneable (the config is behind an `Arc`; the key store is also
/// `Arc`-ed so all handlers share one mtime cache).
#[derive(Clone)]
pub struct AppState {
    /// Runtime configuration (workdir, caps, shell) shared across handlers.
    pub config: Arc<RuntimeConfig>,
    /// Per-session Bearer key store (shared, cached mtime source).
    pub key_store: Arc<SessionKeyStore>,
    /// In-process PTY terminal sessions (`/api/terminals`).
    pub terminals: Arc<TerminalRegistry>,
    /// D9 Prometheus metrics catalogue + registry (one per process).
    pub metrics: Arc<RuntimeMetrics>,
    /// Shared localhost HTTP client for `/proxy/{port}` (upstream's reused
    /// `httpx.AsyncClient`: 300 s total / 5 s connect, redirects off).
    pub proxy_client: reqwest::Client,
    /// Desktop-sidecar HTTP client: 5 s connect timeout but **no total
    /// timeout** — sidecar responses may stream long (desktop assets,
    /// screenshots, downloads) and must not be cut mid-stream.
    pub desktop_client: reqwest::Client,
    /// Single-flight cache of the sidecar's `GET /api/config` (short TTL so
    /// client bursts don't each hit the sidecar; held across the fetch so
    /// concurrent misses coalesce into one upstream request).
    pub desktop_config_cache: Arc<tokio::sync::Mutex<Option<DesktopConfigCache>>>,
}

impl AppState {
    /// Build an `AppState` from its config and key store, wiring the metrics registry
    /// and an empty terminal registry.
    pub fn new(config: RuntimeConfig, key_store: SessionKeyStore) -> Self {
        Self {
            config: Arc::new(config),
            key_store: Arc::new(key_store),
            terminals: Arc::new(TerminalRegistry::new()),
            metrics: RuntimeMetrics::new(),
            proxy_client: reqwest_proxy_client(),
            desktop_client: desktop_proxy_client(),
            desktop_config_cache: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

/// The shared localhost proxy client — upstream `httpx.AsyncClient(timeout=
/// Timeout(300.0, connect=5.0), follow_redirects=False)` parity (upstream
/// `/proxy/{port}`, open_terminal/main.py). No TLS: localhost HTTP only.
fn reqwest_proxy_client() -> reqwest::Client {
    // Upstream's `httpx.Timeout(300.0, connect=5.0)`.
    const TOTAL_TIMEOUT_SECS: u64 = 300;
    const CONNECT_TIMEOUT_SECS: u64 = 5;
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TOTAL_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        // reqwest's default policy is `none` — redirects surface to the client
        // exactly like upstream's `follow_redirects=False`.
        .build()
        .unwrap_or_default()
}

/// The desktop-sidecar client: connect timeout only — deliberately **no total
/// timeout**, so long-running sidecar responses (VNC/desktop asset streams,
/// screenshots, file downloads) are never cut mid-stream. Redirects stay off
/// for parity with the `/proxy/{port}` client.
fn desktop_proxy_client() -> reqwest::Client {
    const CONNECT_TIMEOUT_SECS: u64 = 5;
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_default()
}
