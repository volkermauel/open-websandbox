//! `files::tools` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::{base_of, file_response};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::safe_path;
use crate::state::AppState;

// --- /ports -----------------------------------------------------------------

/// `GET /ports` — report host ports (always empty under the restricted runtime).
#[utoipa::path(
    get,
    path = "/ports",
    tag = "ports",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Host ports (restricted runtime → always empty)", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn list_ports(_auth: Authed) -> Json<serde_json::Value> {
    // Restricted runtime: no host-port introspection. Surface an empty list so the
    // UI ports panel renders cleanly (matches open-terminal's restricted fallback).
    Json(serde_json::json!({ "ports": [] }))
}

// --- /download/{*file_path} -------------------------------------------------

/// Download a workspace-relative file as a byte attachment.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace,
/// [`ApiError::NotFound`] if it is missing or not a regular file, and
/// [`ApiError::Internal`] on a read failure.
#[utoipa::path(
    get,
    path = "/download/{file_path}",
    tag = "tools",
    params(("file_path" = String, Path, description = "Workspace-relative file path")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Raw file bytes (attachment)"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
pub async fn tool_download(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(file_path): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(file_path.trim(), &base)?;
    if !full.is_file() {
        return Err(ApiError::NotFound("File not found".to_string()));
    }
    file_response(&full)
}

// --- /list/{*file_path} -----------------------------------------------------

/// List the entries of a workspace-relative directory.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace and
/// [`ApiError::NotFound`] if it is missing or not a directory.
#[utoipa::path(
    get,
    path = "/list/{file_path}",
    tag = "tools",
    params(("file_path" = String, Path, description = "Workspace-relative directory path")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Directory entries", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Directory not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
// reason: the body delegates to a sync helper, but axum's `Handler` trait
// requires an `async fn`; there is nothing to `.await`.
#[allow(clippy::unused_async)]
pub async fn tool_list(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(file_path): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_impl(&state, &headers, &file_path)
}

/// `GET /list` + `GET /list/` — list the workspace root. The route
/// `/list/{file_path:path}` matches the empty path (lists root); axum's
/// `/list/{*file_path}` catch-all requires ≥1 segment, so these explicit routes
/// cover the empty-path case (parity, D11).
///
/// # Errors
///
/// Returns [`ApiError::Internal`] if the workspace directory cannot be read.
#[utoipa::path(
    get,
    path = "/list",
    tag = "tools",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Workspace-root entries", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
// reason: the body delegates to a sync helper, but axum's `Handler` trait
// requires an `async fn`; there is nothing to `.await`.
#[allow(clippy::unused_async)]
pub async fn tool_list_root(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_impl(&state, &headers, ".")
}

fn list_impl(
    state: &AppState,
    headers: &HeaderMap,
    file_path: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(state, headers)?;
    let fp = file_path.trim();
    let fp = if fp.is_empty() { "." } else { fp };
    let resolved = safe_path(fp, &base)?;
    if !resolved.is_dir() {
        return Err(ApiError::NotFound("Directory not found".to_string()));
    }
    let read = match std::fs::read_dir(&resolved) {
        Ok(r) => r,
        Err(e) => return Err(ApiError::Internal(format!("list failed: {e}"))),
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut entries = Vec::new();
    for n in names {
        let p = resolved.join(&n);
        // os.stat (follows symlinks); broken symlink → skip (TOCTOU/OSError).
        if let Ok(st) = std::fs::metadata(&p) {
            entries.push(serde_json::json!({
                "name": n,
                "is_dir": st.is_dir(),
                "size": st.len(),
            }));
        }
    }
    Ok(Json(
        serde_json::json!({ "path": resolved, "entries": entries }),
    ))
}

// --- /exists/{*file_path} ---------------------------------------------------

/// Probe existence / type of a workspace-relative path.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace.
#[utoipa::path(
    get,
    path = "/exists/{file_path}",
    tag = "tools",
    params(("file_path" = String, Path, description = "Workspace-relative path")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Existence probe", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn tool_exists(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(file_path): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let fp = file_path.trim();
    let fp = if fp.is_empty() { "." } else { fp };
    let full = safe_path(fp, &base)?;
    Ok(Json(serde_json::json!({
        "exists": full.exists(),
        "is_file": full.is_file(),
        "is_dir": full.is_dir(),
    })))
}
