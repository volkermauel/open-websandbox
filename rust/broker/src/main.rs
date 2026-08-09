//! open-websandbox broker binary entry point.
//!
//! Boots the tokio runtime, loads the env-driven config (D12), refuses to start
//! without a configured shared secret (fail-closed, mirroring the Python
//! `_validate_config`), builds the kube-rs client, and serves the axum router
//! on `0.0.0.0:8080`.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use broker::{build_client, build_router, AppState, KubeSandboxStore, ServerConfig};
use shared::{is_placeholder_secret, BrokerConfig};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "broker=info".into()),
        )
        .init();

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
        "broker booting"
    );

    let store = Arc::new(KubeSandboxStore::new(kube_client, cfg.runtime_ns.clone()));
    let state = AppState::new(cfg, store);
    let app = build_router(state);
    let server = ServerConfig::from_env();

    let listener = match tokio::net::TcpListener::bind(server.addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind {}: {e}", server.addr);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("broker listening on {}", server.addr);
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("serve failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
