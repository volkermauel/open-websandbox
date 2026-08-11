//! Shared types for the open-websandbox Rust control plane.
//!
//! This crate is `#![forbid(unsafe_code)]`: no `unsafe` may appear anywhere in
//! our authored control plane (D8). It owns the cross-component contract —
//! Kubernetes CRD types ([`kube::CustomResource`]), the per-session-key auth
//! helpers, and the drop-in config parsing (D12) — so that `broker` and
//! `runtime` consume a single, audited set of definitions.
//!
//! PR-A is intentionally representative: the full CRD / OpenAPI type sets and
//! the complete config object arrive in PR-B (runtime) and PR-C (broker).
#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod crd;
pub mod metrics;
pub mod openapi;
pub mod telemetry;

pub use auth::constant_time_eq;
pub use config::{
    is_placeholder_secret, AnyResult, BrokerConfig, ConfigError, Profile, PLACEHOLDER_SECRETS,
};
pub use crd::{
    OperatingMode, PodIpEntry, Sandbox, SandboxCondition, SandboxSpec, SandboxStatus,
    SandboxTemplate, SandboxTemplateSpec, ShutdownPolicy,
};
pub use metrics::{gather, install, HttpMetrics};
pub use openapi::{BearerAddon, ErrorResponse, BEARER_SCHEME};
pub use telemetry::{init as init_telemetry, shutdown as shutdown_telemetry};
