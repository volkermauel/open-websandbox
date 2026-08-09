//! HTTP handlers — the broker's Open WebUI-facing surface + Sandbox CRUD.
//!
//! Two groups, matching the Python `app`:
//!
//! * **Open** (no auth): `GET /healthz`, `GET /readyz`, `GET /metrics`,
//!   `GET /openapi.json`, `GET /docs` — exactly the routes the Python broker
//!   registers without `Security(_auth)`.
//! * **Gated** (shared Bearer via [`Authed`](crate::auth::Authed)):
//!   `GET /api/config`, `GET /api/status` (broker-served, OpenAPI-defined),
//!   the Sandbox lifecycle CRUD (`/api/sandboxes[/{name}]`), and the catch-all
//!   reverse proxy (`/{*path}`).
//!
//! What is real vs stubbed here is enumerated in the module docs of
//! [`crate`]. Request/response shapes match `openapi_spec.py` for the endpoints
//! it defines; the Sandbox CRUD is the broker-internal lifecycle surface this PR
//! introduces (C-2's resolve-on-request flow reuses it).

#![forbid(unsafe_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use shared::{Profile, Sandbox};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::sandbox::{build_sandbox, extract_pod_template};
use crate::state::AppState;
use crate::store::{StoreError, StoreError::*};

// --- broker-served responses (match openapi_spec.py shapes) ----------------

/// `GET /healthz` and `GET /readyz` body: `{"status": "..."}`.
#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
}

/// `GET /api/config` body: feature flags the OWUI terminal UI connection-gates on.
#[derive(Serialize)]
pub struct ConfigResponse {
    features: Features,
}

#[derive(Serialize)]
pub struct Features {
    terminal: bool,
    notebooks: bool,
    desktop: bool,
}

/// `GET /api/status` body: static operator telemetry.
#[derive(Serialize)]
pub struct StatusResponse {
    active_pods: u64,
    max_pods: u64,
    pods: Vec<serde_json::Value>,
}

// --- Sandbox CRUD request ---------------------------------------------------

/// `POST /api/sandboxes` body: instantiate a Sandbox from a template.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSandboxRequest {
    /// Name of the backing `SandboxTemplate` to clone the pod blueprint from.
    pub template_name: String,
    /// Desired `Sandbox` name (DNS-1123 label).
    pub name: String,
    /// Owning user id (recorded as the `broker-user` annotation).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Owning session/chat id (recorded as the `broker-session` annotation).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Persistence profile; defaults to `BROKER_DEFAULT_PROFILE` when omitted.
    #[serde(default)]
    pub profile: Option<Profile>,
}

/// `GET /api/sandboxes` query: optional Kubernetes label selector.
#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default, rename = "labelSelector")]
    pub label_selector: Option<String>,
}

// --- helpers ----------------------------------------------------------------

/// Current epoch seconds (safe; never panics on a pre-epoch clock).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Map a [`StoreError`] onto an [`ApiError`] for the generic (non-create) path.
fn map_store_err(err: StoreError) -> ApiError {
    match err {
        NotFound => ApiError::NotFound("not found".to_string()),
        Conflict => ApiError::Conflict("already exists".to_string()),
        Kube(e) => ApiError::BadGateway(e.to_string()),
    }
}

// --- open routes ------------------------------------------------------------

/// `GET /healthz` — broker process is up (always 200; never proxied).
pub async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// `GET /readyz` — the apiserver (hard dependency for sandbox resolution) is
/// reachable. 503 when not, so the Service stops routing to a broker that would
/// only 500.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    if state.store.apiserver_reachable().await {
        Ok(Json(HealthResponse { status: "ready" }))
    } else {
        Err(ApiError::ServiceUnavailable(
            "apiserver unreachable".to_string(),
        ))
    }
}

/// `GET /metrics` — Prometheus exposition. PR-C-1 serves a minimal stub so the
/// scrape path exists and is open; PR-C-3 wires the `open_websandbox_broker_*`
/// metric set (D9).
pub async fn metrics() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        "# open-websandbox broker metrics (stub; PR-C-3 wires Prometheus)\n",
    )
        .into_response()
}

/// `GET /openapi.json` — the curated method surface OWUI discovers tools from.
/// PR-C-1 serves a minimal valid document; PR-D's utoipa generation (D10)
/// replaces it with the full `openapi_spec.py` parity + frozen-snapshot guard.
pub async fn openapi_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "open-websandbox broker",
            "version": crate::version(),
            "description": "Broker-served surface (C-1). The full runtime tool surface lands with utoipa generation (D10)."
        },
        "paths": {
            "/healthz": {"get": {"summary": "Broker liveness", "responses": {"200": {"description": "Broker alive"}}}},
            "/readyz": {"get": {"summary": "Broker readiness", "responses": {"200": {"description": "Ready"}, "503": {"description": "Apiserver unreachable"}}}},
            "/api/sandboxes": {
                "get": {"summary": "List sandboxes", "responses": {"200": {"description": "Sandbox list"}}},
                "post": {"summary": "Create a sandbox from a template", "responses": {"201": {"description": "Created"}, "200": {"description": "Already existed"}}}
            },
            "/api/sandboxes/{name}": {
                "get": {"summary": "Get a sandbox", "responses": {"200": {"description": "Sandbox"}, "404": {"description": "Not found"}}},
                "delete": {"summary": "Delete a sandbox", "responses": {"204": {"description": "Deleted"}}}
            }
        }
    }))
}

/// `GET /docs` — Swagger UI shell pointing at `/openapi.json`.
pub async fn docs() -> Response {
    let html = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
        <title>open-websandbox broker</title>\
        <link rel=\"stylesheet\" href=\"https://unpkg.com/swagger-ui-dist@5/swagger-ui.css\">\
        </head><body><div id=\"swagger-ui\"></div>\
        <script src=\"https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js\"></script>\
        <script>window.ui=SwaggerUIBundle({url:\"/openapi.json\",dom_id:\"#swagger-ui\"});</script>\
        </body></html>";
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

// --- gated: broker-served (openapi_spec.py parity) --------------------------

/// `GET /api/config` — feature discovery (terminal UI connection gate).
pub async fn api_config(_: Authed) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        features: Features {
            terminal: true,
            notebooks: false,
            desktop: false,
        },
    })
}

/// `GET /api/status` — operator telemetry (static until C-2/C-4 make it real).
pub async fn api_status(_: Authed) -> Json<StatusResponse> {
    Json(StatusResponse {
        active_pods: 0,
        max_pods: 10,
        pods: Vec::new(),
    })
}

// --- gated: Sandbox lifecycle CRUD ------------------------------------------

/// `POST /api/sandboxes` — create a `Sandbox` from a `SandboxTemplate` (C-1
/// foundation; C-2's resolve-on-request reuses this path). 201 on create, 200
/// with the existing object on a 409 (idempotent, like the Python broker).
pub async fn create_sandbox(
    _: Authed,
    State(state): State<AppState>,
    Json(req): Json<CreateSandboxRequest>,
) -> Result<(StatusCode, Json<Sandbox>), ApiError> {
    let profile = req.profile.unwrap_or(state.config.default_profile);

    let template = state
        .store
        .get_template(&req.template_name)
        .await
        .map_err(map_store_err)?
        .ok_or_else(|| ApiError::NotFound(format!("template {} not found", req.template_name)))?;

    let pod_template = extract_pod_template(&template)?;
    let sandbox = build_sandbox(
        &req.name,
        req.user_id.as_deref(),
        req.session_id.as_deref(),
        profile,
        pod_template,
        &state.config.runtime_ns,
        now_unix(),
    );

    match state.store.create_sandbox(sandbox).await {
        Ok(created) => Ok((StatusCode::CREATED, Json(created))),
        Err(Conflict) => {
            // Idempotent: a concurrent create won — return the existing object.
            let existing = state
                .store
                .get_sandbox(&req.name)
                .await
                .map_err(map_store_err)?
                .ok_or_else(|| {
                    ApiError::Internal(format!("sandbox {} vanished mid-create", req.name))
                })?;
            Ok((StatusCode::OK, Json(existing)))
        }
        Err(other) => Err(map_store_err(other)),
    }
}

/// `GET /api/sandboxes` — list broker-owned sandboxes.
pub async fn list_sandboxes(
    _: Authed,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Sandbox>>, ApiError> {
    let items = state
        .store
        .list_sandboxes(q.label_selector.as_deref())
        .await
        .map_err(map_store_err)?;
    Ok(Json(items))
}

/// `GET /api/sandboxes/{name}` — get a single sandbox (status included).
pub async fn get_sandbox(
    _: Authed,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Sandbox>, ApiError> {
    state
        .store
        .get_sandbox(&name)
        .await
        .map_err(map_store_err)?
        .ok_or_else(|| ApiError::NotFound(format!("sandbox {name} not found")))
        .map(Json)
}

/// `DELETE /api/sandboxes/{name}` — delete a sandbox (404-tolerant → 204).
pub async fn delete_sandbox(
    _: Authed,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .store
        .delete_sandbox(&name)
        .await
        .map_err(map_store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `/{*path}` catch-all reverse proxy. C-1 returns 501: the resolve-on-request
/// flow + runtime hop forwarding lands in PR-C-2.
pub async fn proxy_catch_all(_: Authed) -> ApiError {
    ApiError::NotImplemented(
        "reverse-proxy not implemented in PR-C-1 (lands in PR-C-2)".to_string(),
    )
}
