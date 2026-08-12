//! open-websandbox broker binary entry point.
//!
//! Boots the tokio runtime, loads the env-driven config (D12), refuses to start
//! without a configured shared secret (fail-closed boot guard), builds the
//! kube-rs client, serves the axum router on
//! `0.0.0.0:8080`, and spawns the PR-C-3 background tasks: leader election
//! (`run_leader_loop`, maintains [`LeaderGate`] + releases the lease on shutdown)
//! and the leader-gated idle reaper (`run_reaper_loop`, no-ops while not leader).
//! Graceful shutdown (SIGTERM/SIGINT) drains the HTTP server, then cancels both
//! tasks so the leader steps down (releases the lease) cleanly.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use broker::leaser::{run_leader_loop, KubeLease, LeaderGate, LeaseClient};
use broker::reaper::{run_reaper_loop, NoopOffload, ReapOffload};
use broker::{
    build_client, build_router, AppState, AwsColdStore, KubeSandboxStore, S3Offload, SandboxStore,
    ServerConfig,
};
use shared::{is_placeholder_secret, BrokerConfig};
use tokio::signal;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> ExitCode {
    // rustls 0.23 panics ("Could not automatically determine the process-level
    // CryptoProvider") when multiple TLS-using crates (kube-rs, reqwest
    // rustls-tls, aws-sdk-s3) pull it in without one being installed. aws-lc-rs
    // is the rustls 0.23 default + what aws-sdk-s3 uses natively; install it
    // before any TLS handshake (kube client build, first proxied request, first
    // S3 offload). install_default is idempotent — a later caller (e.g. aws-sdk)
    // silently no-ops onto the already-installed provider.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // D9 — soft OTel: fmt always; OTLP/gRPC bridge only when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set (no-op otherwise). Held for the
    // process lifetime so the batch processor can flush on shutdown.
    let otel_provider = shared::init_telemetry("open-websandbox-broker", "broker=info");

    let cfg = match BrokerConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("broker config invalid: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Fail-closed boot guard: an unset/placeholder secret would silently
    // disable auth (every request path also re-checks), so refuse to start.
    if is_placeholder_secret(&cfg.shared_secret) {
        tracing::error!(
            "BROKER_SHARED_SECRET is unset or a known placeholder — refusing to start. \
             Set a strong secret (the Helm chart auto-generates one)."
        );
        return ExitCode::FAILURE;
    }

    let kube_client = match build_client().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to build Kubernetes client: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        namespace = %cfg.runtime_ns,
        base_template = %cfg.base_template,
        default_profile = cfg.default_profile.as_str(),
        leader_lease = %cfg.leader_lease,
        "broker booting"
    );

    // cfg_arc backs the background tasks (the request path gets its own clone
    // inside AppState); store is shared by the request path + the reaper.
    let cfg_arc = Arc::new(cfg.clone());
    let store: Arc<dyn SandboxStore> = Arc::new(KubeSandboxStore::new(
        kube_client.clone(),
        cfg_arc.runtime_ns.clone(),
    ));
    let state = AppState::new(cfg, store.clone());
    // C-4 cold tier: build the S3 driver once when `broker.s3.enabled`; the
    // SAME instance offloads on reap (leader-gated reaper) and restores on
    // resume (resolve). When disabled the reaper keeps [`NoopOffload`] and
    // resolve skips the restore hop (state.s3_restore == None).
    let (offload, state) = if cfg_arc.s3_enabled {
        let cold = Arc::new(AwsColdStore::new(&cfg_arc));
        let s3 =
            Arc::new(S3Offload::new(&cfg_arc, cold, state.http.clone()).with_store(store.clone()));
        let restore = Arc::clone(&s3);
        (s3 as Arc<dyn ReapOffload>, state.with_s3_restore(restore))
    } else {
        (Arc::new(NoopOffload) as Arc<dyn ReapOffload>, state)
    };

    let server = ServerConfig::from_env();

    // --- PR-C-3 background: leader election + leader-gated idle reaper --------
    let gate = Arc::new(LeaderGate::new());
    let lease: Arc<dyn LeaseClient> = Arc::new(KubeLease::new(kube_client, &cfg_arc));
    let app = build_router(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let leader_handle = {
        let lease = lease.clone();
        let gate = gate.clone();
        let rx = shutdown_rx.clone();
        let renew_interval = Duration::from_secs(cfg_arc.leader_renew_seconds.max(1));
        tokio::spawn(async move {
            run_leader_loop(lease, gate, renew_interval, rx).await;
        })
    };
    let reaper_handle = {
        let store = store.clone();
        let offload = offload.clone();
        let cfg = cfg_arc.clone();
        let gate = gate.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            run_reaper_loop(gate, store, offload, cfg, rx).await;
        })
    };

    let listener = match tokio::net::TcpListener::bind(server.addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {}: {e}", server.addr);
            // Release the lease so a peer can take over immediately.
            let _ = shutdown_tx.send(true);
            let _ = leader_handle.await;
            let _ = reaper_handle.await;
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("broker listening on {}", server.addr);

    // Serve until a shutdown signal arrives, then drain in-flight requests.
    let serve = {
        let shutdown_tx = shutdown_tx.clone();
        axum::serve(listener, app).with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Signal the background tasks to step down before we await them.
            let _ = shutdown_tx.send(true);
        })
    };
    if let Err(e) = serve.await {
        tracing::error!("serve failed: {e}");
        let _ = shutdown_tx.send(true);
        let _ = leader_handle.await;
        let _ = reaper_handle.await;
        return ExitCode::FAILURE;
    }

    // HTTP server drained: wait for the reaper to stop + the leader to release
    // its lease (clean step-down — a peer wins the next election without waiting
    // for `leader_duration_seconds` to elapse).
    tracing::info!("broker draining background tasks");
    let _ = reaper_handle.await;
    let _ = leader_handle.await;
    tracing::info!("broker shutdown complete");
    // D9 — best-effort flush of the OTLP batch processor on graceful exit.
    shared::shutdown_telemetry(otel_provider);
    ExitCode::SUCCESS
}

/// Wait for SIGTERM (k8s pod termination) or SIGINT (ctrl-c). Standard axum
/// graceful-shutdown signal future.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to install CTRL-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => { tracing::info!("shutdown signal: ctrl-c"); }
        () = terminate => { tracing::info!("shutdown signal: SIGTERM"); }
    }
}
