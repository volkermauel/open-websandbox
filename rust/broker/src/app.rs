//! Router construction. Split out of `main.rs` so the integration tests can
//! build an in-process app and exercise it via `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::routing::{any, get, post};
use axum::Router;

use crate::api::{
    api_config, api_status, create_sandbox, delete_sandbox, docs, get_sandbox, healthz,
    list_sandboxes, metrics, openapi_json, proxy_catch_all, readyz,
};
use crate::state::AppState;

/// Build the full broker router over `state`.
///
/// Open probes (`/healthz`, `/readyz`, `/metrics`, `/openapi.json`, `/docs`) and
/// the gated surface share one state: handlers that don't need it simply omit
/// the [`State`](axum::extract::State) extractor, while every gated handler
/// declares [`Authed`](crate::auth::Authed) first so each is individually
/// fail-closed (mirrors the Python per-route `Security(_auth)`).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Open (unauthenticated) routes.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(docs))
        // Gated (shared Bearer) broker-served routes.
        .route("/api/config", get(api_config))
        .route("/api/status", get(api_status))
        // Sandbox lifecycle CRUD.
        .route("/api/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route(
            "/api/sandboxes/{name}",
            get(get_sandbox).delete(delete_sandbox),
        )
        // Catch-all reverse proxy (C-1 stub → 501; PR-C-2 implements forwarding).
        .route("/{*path}", any(proxy_catch_all))
        .with_state(state)
}
