//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::store::SandboxStore;

/// State shared across all request handlers.
///
/// The [`BrokerConfig`](shared::BrokerConfig) is the env-driven drop-in config
/// (D12); the [`SandboxStore`] is the (real or stubbed) Kubernetes lifecycle
/// backend, type-erased so the HTTP handlers can be exercised in-process
/// against an in-memory store without a live cluster.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<shared::BrokerConfig>,
    pub store: Arc<dyn SandboxStore>,
}

impl AppState {
    /// Build state from a config and a store backend.
    pub fn new(config: shared::BrokerConfig, store: Arc<dyn SandboxStore>) -> Self {
        Self {
            config: Arc::new(config),
            store,
        }
    }
}
