//! open-websandbox broker — the Open WebUI front door + Sandbox lifecycle.
//!
//! PR-C-1 delivers the HTTP foundation and the Kubernetes `Sandbox` CRUD this
//! crate is built on. The broker's request path is, in outline, the same as the
//! Python original (`broker/main.py`): authenticate the shared Bearer secret →
//! resolve-or-create the per-session `Sandbox` → reverse-proxy to the runtime
//! pod. This chunk makes the broker-served HTTP surface, the auth guard, the
//! kube-rs client, and the `Sandbox` lifecycle CRUD real and tested; the
//! resolve-on-request reverse proxy, leader election, reaper, warm pool, S3
//! tiering, per-session key management, and metrics land in later PRs (C-2 →
//! C-4).
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
pub mod sandbox;
pub mod state;
pub mod store;

pub use app::build_router;
pub use client::build_client;
pub use config::ServerConfig;
pub use error::ApiError;
pub use sandbox::{build_sandbox, extract_pod_template};
pub use shared;
pub use state::AppState;
pub use store::{KubeSandboxStore, SandboxStore, StoreError, StubSandboxStore};

/// Crate version ( surfaced for `/healthz`-style diagnostics).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
