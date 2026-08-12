//! open-websandbox broker — the Open WebUI front door + Sandbox lifecycle.
//!
//! The broker's request path is the canonical one:
//! authenticate the shared Bearer secret → [`resolve`] the per-session `Sandbox`
//! (get-or-create + wait for `Ready`) → [`proxy`] the runtime-tool request to the
//! resolved pod. PR-C-1 landed the HTTP foundation, auth guard, kube-rs client,
//! and `Sandbox` lifecycle CRUD; PR-C-2 (this code) adds the resolve-on-request
//! flow + the reverse proxy + the terminal WebSocket relay. Leader election,
//! idle reaper, warm pool, S3 tiering, per-session-key Secret injection/rotation,
//! and metrics land in C-3/C-4.
//!
//! `#![forbid(unsafe_code)]` holds across the whole crate (D8): every memory-
//! safety guarantee comes from the type system and the audited dependency
//! tree, never from a hand-written `unsafe` block here.

#![forbid(unsafe_code)]

pub mod api;
pub mod app;
pub mod auth;
pub mod client;
pub mod config;
pub mod error;
pub mod leaser;
pub mod metrics;
pub mod proxy;
pub mod rate_limit;
pub mod reaper;
pub mod resolve;
pub mod runtime_key;
pub mod s3;
pub mod sandbox;
pub mod state;
pub mod store;
pub mod terminal;

pub mod openapi;
pub use app::build_router;
pub use client::build_client;
pub use config::ServerConfig;
pub use error::ApiError;
#[cfg(test)]
pub use leaser::InMemoryLeaseClient;
pub use leaser::{InMemoryLease, KubeLease, LeaderGate, LeaseClient};
pub use metrics::BrokerMetrics;
pub use reaper::{NoopOffload, OffloadError, ReapOffload};
pub use resolve::{resolve_sandbox, sandbox_name, ResolvedSandbox};
pub use s3::{
    s3_namespace, s3_object_key, AwsColdStore, ColdError, ColdStore, RestoreError, RestoreOutcome,
    S3Offload,
};
pub use sandbox::{build_sandbox, extract_pod_template};
pub use shared;
pub use state::AppState;
pub use store::{KubeSandboxStore, SandboxStore, StoreError};

/// Test-only store/lease doubles, re-exported under one clearly-named namespace so
/// integration tests in `tests/` can reuse them without polluting the production
/// module surface. (`InMemoryLeaseClient` is `#[cfg(test)]`-only — same-file unit
/// tests — so it is not re-exported here.)
pub mod test_fakes {
    pub use crate::s3::test_fakes::InMemoryColdStore;
    pub use crate::store::test_fakes::StubSandboxStore;
}

/// Crate version ( surfaced for `/healthz`-style diagnostics).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
