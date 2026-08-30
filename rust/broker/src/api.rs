//! HTTP handlers — the broker's Open WebUI-facing surface + Sandbox CRUD.
//!
//! Two route groups:
//!
//! * **Open** (no auth): `GET /healthz`, `GET /readyz`, `GET /metrics`,
//!   `GET /openapi.json`, `GET /docs` — registered without an auth guard.
//! * **Gated** (shared Bearer via `Authed`):
//!   `GET /api/config`, `GET /api/status` (broker-served, OpenAPI-defined),
//!   the Sandbox lifecycle CRUD (`/api/sandboxes[/{name}]`), and the catch-all
//!   reverse proxy (`/{*path}`).
//!
//! What is real vs stubbed here is enumerated in the module docs of
//! [`crate`]. Request/response shapes match the `OpenAPI` spec for the endpoints
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
use utoipa::{IntoParams, ToSchema};

use shared::{Profile, Sandbox};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::metrics::{SANDBOXES_CREATED_TOTAL, SANDBOXES_DELETED_TOTAL};
use crate::sandbox::{build_sandbox, extract_pod_template};
use crate::state::AppState;
use crate::store::{
    StoreError,
    StoreError::{Conflict, Kube, NotFound},
};

// --- broker-served responses (match the OpenAPI shapes) -------------------

/// `GET /healthz` and `GET /readyz` body: `{"status": "..."}`.
#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    status: &'static str,
}

/// `GET /api/config` body: feature flags the OWUI terminal UI connection-gates on.
#[derive(Serialize, ToSchema)]
pub struct ConfigResponse {
    features: Features,
}

/// Feature-flag set returned inside `ConfigResponse`.
#[derive(Serialize, ToSchema)]
pub struct Features {
    terminal: bool,
    notebooks: bool,
    system: bool,
}

/// `GET /api/status` body: static operator telemetry.
#[derive(Serialize, ToSchema)]
pub struct StatusResponse {
    active_pods: u64,
    max_pods: u64,
    pods: Vec<serde_json::Value>,
}

// --- Sandbox CRUD request ---------------------------------------------------

/// `POST /api/sandboxes` body: instantiate a Sandbox from a template.
#[derive(Deserialize, ToSchema)]
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

/// `OpenAPI` schema for a `Sandbox` object on the wire (`apiVersion`/`kind`/`metadata`/
/// `spec`/`status`). The CRUD handlers return the kube-generated [`shared::Sandbox`] (an
/// identical shape); this broker-local schema surfaces the typed spec/status in the
/// generated document (issue #75).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SandboxObject {
    /// `agents.x-k8s.io/v1beta1`.
    pub api_version: String,
    /// `Sandbox`.
    pub kind: String,
    /// Kubernetes `ObjectMeta` (name, namespace, labels, annotations, …).
    pub metadata: serde_json::Value,
    /// Sandbox spec (template, profile, operating mode, …).
    pub spec: shared::SandboxSpec,
    /// Last-observed status (conditions, pod IPs, …); absent until reconciled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<shared::SandboxStatus>,
}

/// `GET /api/sandboxes` query: optional Kubernetes label selector.
#[derive(Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Kubernetes label selector (URL `labelSelector`) to filter listed sandboxes.
    #[serde(default, rename = "labelSelector")]
    pub label_selector: Option<String>,
}

// --- helpers ----------------------------------------------------------------

/// Current epoch seconds (safe; never panics on a pre-epoch clock).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses(
        (status = 200, description = "Broker process is up", body = HealthResponse)
    )
)]
pub async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

/// `GET /readyz` — the apiserver (hard dependency for sandbox resolution) is
/// reachable. 503 when not, so the Service stops routing to a broker that would
/// only 500.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "health",
    responses(
        (status = 200, description = "Apiserver reachable", body = HealthResponse),
        (status = 503, description = "Apiserver unreachable", body = shared::ErrorResponse)
    )
)]
pub async fn readyz(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    if state.store.apiserver_reachable().await {
        Ok(Json(HealthResponse { status: "ready" }))
    } else {
        Err(ApiError::ServiceUnavailable(
            "apiserver unreachable".to_string(),
        ))
    }
}

/// `GET /metrics` — Prometheus exposition (D9). Renders the broker's full
/// metric catalogue (`open_websandbox_broker_*`: HTTP rate/latency, active
/// sandboxes, sandbox create/delete counts, runtime-hop errors) in
/// `text/plain; version=0.0.4`.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "health",
    responses(
        (status = 200, description = "Prometheus exposition (text/plain)")
    )
)]
pub async fn metrics() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        shared::gather(),
    )
        .into_response()
}

/// `GET /openapi.json` — the curated method surface OWUI discovers tools from.
///
/// Generated by utoipa (D10) from the broker + runtime handler annotations, merged into
/// one document. `info.version` is pinned to the crate version (issue #75 Q4) and the
/// structural shape is guarded by a frozen-snapshot test (issue #75 Q2).
#[utoipa::path(
    get,
    path = "/openapi.json",
    tag = "docs",
    responses(
        (status = 200, description = "OpenAPI 3 document (JSON)")
    )
)]
pub async fn openapi_json() -> Json<serde_json::Value> {
    // Round-trip through `serde_json::Value` so the object keys are BTreeMap-sorted
    // (deterministic regardless of utoipa's insertion order); the frozen-snapshot test
    // asserts byte-equality against a committed fixture.
    let doc = crate::openapi::openapi_document();
    let value = serde_json::to_value(&doc).expect("OpenApi document serializes to JSON");
    Json(value)
}

/// `GET /docs` — the Scalar API reference UI, served by `utoipa-scalar` and inlining
/// `/openapi.json` (issue #75 Q3).
#[utoipa::path(
    get,
    path = "/docs",
    tag = "docs",
    responses(
        (status = 200, description = "Scalar API reference UI (HTML)")
    )
)]
pub async fn docs() -> Response {
    let html = utoipa_scalar::Scalar::new(crate::openapi::openapi_document()).to_html();
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

// --- gated: broker-served (OpenAPI parity) ---------------------------------

/// `GET /api/config` — feature discovery (terminal UI connection gate).
#[utoipa::path(
    get,
    path = "/api/config",
    tag = "broker",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Feature discovery flags", body = ConfigResponse),
        (status = 401, description = "Missing/invalid shared Bearer", body = shared::ErrorResponse),
        (status = 503, description = "Shared secret not configured (fail-closed)", body = shared::ErrorResponse)
    )
)]
pub async fn api_config(_: Authed) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        features: Features {
            terminal: true,
            notebooks: false,
            // Stage 2 (#169): GET /system is now served by the runtime.
            system: true,
        },
    })
}

/// `GET /api/status` — operator telemetry (static until C-2/C-4 make it real).
#[utoipa::path(
    get,
    path = "/api/status",
    tag = "broker",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Operator telemetry (static)", body = StatusResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 503, body = shared::ErrorResponse)
    )
)]
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
/// with the existing object on a 409 — idempotent (409 → update existing).
#[utoipa::path(
    post,
    path = "/api/sandboxes",
    tag = "sandboxes",
    request_body = CreateSandboxRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 201, description = "Sandbox created", body = SandboxObject),
        (status = 200, description = "Already existed (idempotent create)", body = SandboxObject),
        (status = 400, description = "Malformed request", body = shared::ErrorResponse),
        (status = 401, description = "Missing/invalid shared Bearer", body = shared::ErrorResponse),
        (status = 404, description = "Template not found", body = shared::ErrorResponse),
        (status = 502, description = "Apiserver rejected the call", body = shared::ErrorResponse),
        (status = 503, description = "Shared secret not configured", body = shared::ErrorResponse)
    )
)]
#[tracing::instrument(name = "sandbox.create", skip(state, req), fields(sandbox = tracing::field::Empty))]
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
        Ok(created) => {
            // D9: a brand-new sandbox was created (explicit POST path).
            metrics::counter!(SANDBOXES_CREATED_TOTAL).increment(1);
            Ok((StatusCode::CREATED, Json(created)))
        }
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
#[utoipa::path(
    get,
    path = "/api/sandboxes",
    tag = "sandboxes",
    params(ListQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Broker-owned sandboxes", body = Vec<SandboxObject>),
        (status = 401, body = shared::ErrorResponse),
        (status = 502, body = shared::ErrorResponse),
        (status = 503, body = shared::ErrorResponse)
    )
)]
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
#[utoipa::path(
    get,
    path = "/api/sandboxes/{name}",
    tag = "sandboxes",
    params(("name" = String, Path, description = "Sandbox name (DNS-1123 label)")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Sandbox (status included)", body = SandboxObject),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Sandbox not found", body = shared::ErrorResponse),
        (status = 502, body = shared::ErrorResponse),
        (status = 503, body = shared::ErrorResponse)
    )
)]
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
#[utoipa::path(
    delete,
    path = "/api/sandboxes/{name}",
    tag = "sandboxes",
    params(("name" = String, Path, description = "Sandbox name")),
    security(("brokerBearer" = [])),
    responses(
        (status = 204, description = "Deleted (404-tolerant → 204)"),
        (status = 401, body = shared::ErrorResponse),
        (status = 502, body = shared::ErrorResponse),
        (status = 503, body = shared::ErrorResponse)
    )
)]
#[tracing::instrument(name = "sandbox.delete", skip(state), fields(sandbox = %name))]
pub async fn delete_sandbox(
    _: Authed,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    // D9: only count an actual delete (the store is 404-tolerant: a sandbox
    // already gone yields `false`).
    if state
        .store
        .delete_sandbox(&name)
        .await
        .map_err(map_store_err)?
    {
        metrics::counter!(SANDBOXES_DELETED_TOTAL).increment(1);
    }
    Ok(StatusCode::NO_CONTENT)
}
