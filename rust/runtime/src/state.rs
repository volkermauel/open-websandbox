//! Shared application state carried by every handler via `axum::extract::State`.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::auth::SessionKeyStore;
use crate::config::RuntimeConfig;

/// State shared across all request handlers.
///
/// Cheaply cloneable (the config is behind an `Arc`; the key store is also
/// `Arc`-ed so all handlers share one mtime cache).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RuntimeConfig>,
    pub key_store: Arc<SessionKeyStore>,
}

impl AppState {
    pub fn new(config: RuntimeConfig, key_store: SessionKeyStore) -> Self {
        Self {
            config: Arc::new(config),
            key_store: Arc::new(key_store),
        }
    }
}
