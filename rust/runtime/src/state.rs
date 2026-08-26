//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::auth::SessionKeyStore;
use crate::config::RuntimeConfig;
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
        }
    }
}
