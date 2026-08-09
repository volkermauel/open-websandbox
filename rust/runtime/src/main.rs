//! open-websandbox runtime binary entry point.
//!
//! Boots the tokio runtime, loads config from the environment (D12), applies the
//! per-sandbox `RLIMIT_NPROC` cap (best-effort, inherited by spawned children),
//! refuses to start without a per-session key (fail-closed), and serves the axum
//! router on `0.0.0.0:8888`.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;

use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "runtime=info".into()),
        )
        .init();

    let cfg = RuntimeConfig::from_env();
    tracing::info!(workdir = %cfg.workdir.display(), "runtime booting");

    // Best-effort per-sandbox process cap; inherited by every spawned child.
    apply_max_procs(cfg.max_procs);

    let key_store = SessionKeyStore::new(cfg.runtime_key_file.clone());
    if let Err(e) = key_store.validate() {
        tracing::error!("{e}");
        return ExitCode::from(1);
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], 8888));

    let state = AppState::new(cfg, key_store);
    let app = build_router(state);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {addr} failed: {e}");
            return ExitCode::from(1);
        }
    };
    tracing::info!("runtime listening on {addr}");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("serve failed: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Apply `RLIMIT_NPROC` to this process (and thus every child) — best-effort,
/// matching the Python runtime's `try/except` swallow. Logged, never fatal.
fn apply_max_procs(max_procs: u64) {
    use nix::sys::resource::{setrlimit, Resource};
    match setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs) {
        Ok(()) => tracing::info!("RLIMIT_NPROC set to {max_procs}"),
        Err(e) => tracing::warn!("could not set RLIMIT_NPROC (best-effort): {e}"),
    }
}
