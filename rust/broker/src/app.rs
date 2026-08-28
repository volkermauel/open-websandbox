//! Router construction. Split out of `main.rs` so the integration tests can
//! build an in-process app and exercise it via `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::middleware::from_fn_with_state;
use axum::routing::{any, get, post};
use axum::Router;

use crate::api::{
    api_config, api_status, create_sandbox, delete_sandbox, docs, get_sandbox, healthz,
    list_sandboxes, metrics, openapi_json, readyz,
};
use crate::metrics::http_metrics_layer;
use axum::extract::DefaultBodyLimit;

use crate::proxy::{proxy_catch_all, MAX_FORWARD_BODY};
use crate::rate_limit;
use crate::state::AppState;
use crate::terminal::terminal_ws;

/// Build the full broker router over `state`.
///
/// Open probes (`/healthz`, `/readyz`, `/metrics`, `/openapi.json`, `/docs`) and
/// the gated surface share one state: handlers that don't need it simply omit
/// the [`State`](axum::extract::State) extractor, while every gated handler
/// declares [`Authed`](crate::auth::Authed) first so each is individually
/// fail-closed.
pub fn build_router(state: AppState) -> Router {
    // #98 A3: open probes stay on a separate, UNLIMITED router so kubelet
    // health/readiness and the /metrics scrape are never throttled by the
    // per-user token-bucket. The gated surface (sandboxes, terminals, catch-all
    // proxy for /execute, /files/*, …) carries the GovernorLayer.
    let open: Router<AppState> = Router::new()
        // Open (unauthenticated) routes.
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(docs));
    let gated: Router<AppState> = Router::new()
        // Gated (shared Bearer) broker-served routes.
        .route("/api/config", get(api_config))
        .route("/api/status", get(api_status))
        // Sandbox lifecycle CRUD.
        .route("/api/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route(
            "/api/sandboxes/{name}",
            get(get_sandbox).delete(delete_sandbox),
        )
        // Interactive terminal WebSocket relay (OWUI open-terminal contract).
        // POST /api/terminals (create) still flows through the catch-all proxy.
        .route("/api/terminals/{id}", get(terminal_ws))
        // Catch-all reverse proxy: /execute, /files/*, /snapshot, /restore,
        // /api/terminals (POST), … → resolved runtime pod.
        .route("/{*path}", any(proxy_catch_all))
        // #162: raise axum's 2 MiB default body limit so large uploads can
        // reach the buffered forward (still capped at MAX_FORWARD_BODY).
        .layer(DefaultBodyLimit::max(MAX_FORWARD_BODY));
    let gated = rate_limit::apply(gated, &state);
    open.merge(gated)
        // D9: record HTTP rate/latency for every served request, keyed by the
        //      templated route (bounded-cardinality `path` label).
        .layer(from_fn_with_state(state.clone(), http_metrics_layer))
        .with_state(state)
}
