//! open-websandbox runtime binary entry point.
//!
//! Boots the tokio runtime, loads config from the environment (D12), applies the
//! per-sandbox `RLIMIT_NPROC` cap (best-effort, inherited by spawned children),
//! refuses to start without a per-session key (fail-closed), and serves the axum
//! router on `0.0.0.0:8888`.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;

use runtime::{build_router, terminals, AppState, RuntimeConfig, SessionKeyStore};

#[tokio::main]
async fn main() -> ExitCode {
    // D9 — soft OTel: fmt always; OTLP/gRPC bridge only when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set (no-op otherwise).
    let otel_provider = shared::init_telemetry("open-websandbox-runtime", "runtime=info");

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
    // Idle-detached terminal sweep (issue #129): reaps PTYs whose WS client has
    // been gone longer than TERMINAL_DETACH_TTL_SECS, so detached sessions never
    // leak up to MAX_TERMINAL_SESSIONS.
    let _sweep = terminals::spawn_detached_sweep(state.clone());
    let app = build_router(state.clone());
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {addr} failed: {e}");
            return ExitCode::from(1);
        }
    };
    tracing::info!("runtime listening on {addr}");

    // SIGTERM = pod eviction / drain (issue #129): stop serving and flush every
    // terminal's scrollback under the workspace so the recreated pod can replay
    // it (bounded rings — completes well inside the default 30s grace period).
    // SIGINT gets the same treatment for local runs. WS relays do NOT drain
    // gracefully (they never end) — the select below returns as soon as the
    // signal arrives and the process exits after the flush.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    let served = axum::serve(listener, app);

    tokio::select! {
        r = served => {
            if let Err(e) = r {
                tracing::error!("serve failed: {e}");
                shared::shutdown_telemetry(otel_provider);
                return ExitCode::from(1);
            }
        }
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    let flushed = terminals::flush_scrollbacks(&state).await;
    tracing::info!(sessions = flushed, "terminal scrollback flushed; exiting");
    // D9 — best-effort flush of the OTLP batch processor on graceful exit.
    shared::shutdown_telemetry(otel_provider);
    ExitCode::SUCCESS
}

/// Apply `RLIMIT_NPROC` to this process (and thus every child) — best-effort;
/// failures are swallowed. Logged, never fatal.
fn apply_max_procs(max_procs: u64) {
    use nix::sys::resource::{setrlimit, Resource};
    match setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs) {
        Ok(()) => tracing::info!("RLIMIT_NPROC set to {max_procs}"),
        Err(e) => tracing::warn!("could not set RLIMIT_NPROC (best-effort): {e}"),
    }
}
