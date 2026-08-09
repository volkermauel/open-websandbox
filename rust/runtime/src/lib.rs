//! open-websandbox runtime (axum + tokio): hardened `/execute` + `/files/*`.
//!
//! This crate is `#![forbid(unsafe_code)]`: no `unsafe` appears in our authored
//! code anywhere. The `nix` FFI wrappers (`setrlimit`, `killpg`) and `std`'s
//! `CommandExt::process_group` are themselves safe public APIs — their internal
//! `unsafe` lives inside `nix`/`std`, not in this crate.

#![forbid(unsafe_code)]

pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod execute;
pub mod files;
pub mod safe_path;
pub mod snapshot;
pub mod state;

pub use app::build_router;
pub use auth::SessionKeyStore;
pub use config::RuntimeConfig;
pub use state::AppState;
