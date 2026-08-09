//! `GET|POST /files/*` — the open-terminal filesystem surface.
//!
//! Every handler funnels its caller-supplied path through
//! [`safe_path`](crate::safe_path) first, so traversal/escape attempts come back
//! as HTTP 400 rather than leaking bytes outside the workspace base. Auth is via
//! the [`Authed`] extractor (the per-session key guard).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::{request_base, safe_path};
use crate::state::AppState;

fn subdir_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-workspace-subdir")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

fn base_of(state: &AppState, headers: &HeaderMap) -> Result<PathBuf, ApiError> {
    request_base(&state.config.workdir, subdir_from(headers))
}

/// Convert a file mtime to seconds-since-epoch (Python `float(st.st_mtime)`).
fn modified_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Serialize)]
pub struct Entry {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    size: u64,
    modified: f64,
}

#[derive(Serialize)]
pub struct ListResponse {
    dir: PathBuf,
    entries: Vec<Entry>,
}

// --- /files/cwd --------------------------------------------------------------

pub async fn get_cwd(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "cwd": base,
        "home": base,
    })))
}

#[derive(Deserialize)]
pub struct CwdRequest {
    pub path: String,
}

pub async fn set_cwd(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CwdRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let resolved = safe_path(&req.path, &base)?;
    if !resolved.is_dir() {
        return Err(ApiError::NotFound("Directory not found".to_string()));
    }
    Ok(Json(serde_json::json!({ "cwd": resolved })))
}

// --- /files/list -------------------------------------------------------------

#[derive(Deserialize)]
pub struct ListQuery {
    pub directory: Option<String>,
}

pub async fn list_dir(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    let raw = q.directory.as_deref().unwrap_or(".");
    let normalised = raw.trim().to_ascii_lowercase();
    let directory = match normalised.as_str() {
        "" | "null" => ".",
        other => other,
    };
    let base = base_of(&state, &headers)?;
    let resolved = safe_path(directory, &base)?;
    let meta = std::fs::metadata(&resolved)
        .map_err(|_| ApiError::NotFound("Directory not found".to_string()))?;
    if !meta.is_dir() {
        return Err(ApiError::NotFound("Directory not found".to_string()));
    }
    let read = std::fs::read_dir(&resolved)
        .map_err(|e| ApiError::Internal(format!("list failed: {e}")))?;
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        if let Some(entry) = entry_for(&resolved.join(&name)) {
            entries.push(entry);
        }
    }
    Ok(Json(ListResponse {
        dir: resolved,
        entries,
    }))
}

fn entry_for(p: &Path) -> Option<Entry> {
    let meta = std::fs::symlink_metadata(p).ok()?;
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    // Match Python: `os.path.isdir` follows symlinks, so a symlink to a dir is
    // reported as "directory"; a broken symlink falls back to "file".
    let is_dir = if meta.file_type().is_symlink() {
        std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
    } else {
        meta.is_dir()
    };
    Some(Entry {
        name,
        kind: if is_dir { "directory" } else { "file" },
        size: meta.len(),
        modified: modified_secs(&meta),
    })
}

// --- /files/read -------------------------------------------------------------

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: String,
}

pub async fn read_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Response, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&q.path, &base)?;
    let meta =
        std::fs::metadata(&full).map_err(|_| ApiError::NotFound("File not found".to_string()))?;
    if !meta.is_file() {
        return Err(ApiError::NotFound("File not found".to_string()));
    }
    // Image -> raw bytes with guessed mime (matches open-terminal contract).
    let mime = mime_guess::from_path(&full).first_or_octet_stream();
    if mime.type_() == mime_guess::mime::IMAGE {
        let bytes =
            std::fs::read(&full).map_err(|e| ApiError::Internal(format!("read failed: {e}")))?;
        return Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string())],
            bytes,
        )
            .into_response());
    }
    let content = std::fs::read_to_string(&full)
        .map_err(|e| ApiError::Internal(format!("read failed: {e}")))?;
    let total_lines = content.lines().count();
    Ok(Json(serde_json::json!({
        "path": full,
        "total_lines": total_lines,
        "content": content,
    }))
    .into_response())
}

// --- /files/write ------------------------------------------------------------

#[derive(Deserialize)]
pub struct WriteRequest {
    pub path: String,
    pub content: String,
}

pub async fn write_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<WriteRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&req.path, &base)?;
    let data = req.content.into_bytes();
    if let Some(parent) = full.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
        }
    }
    std::fs::write(&full, &data).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(
        serde_json::json!({ "path": full, "size": data.len() }),
    ))
}

// --- /files/mkdir ------------------------------------------------------------

pub async fn mkdir(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PathBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&req.path, &base)?;
    std::fs::create_dir_all(&full).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(serde_json::json!({ "path": full })))
}

#[derive(Deserialize)]
pub struct PathBody {
    pub path: String,
}

// --- /files/move -------------------------------------------------------------

#[derive(Deserialize)]
pub struct MoveRequest {
    pub source: String,
    pub destination: String,
}

pub async fn move_entry(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MoveRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let src = safe_path(&req.source, &base)?;
    let dst = safe_path(&req.destination, &base)?;
    if !src.exists() {
        return Err(ApiError::NotFound("Source path not found".to_string()));
    }
    if dst.exists() {
        return Err(ApiError::Conflict("Destination already exists".to_string()));
    }
    std::fs::rename(&src, &dst).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(
        serde_json::json!({ "source": src, "destination": dst }),
    ))
}

// --- /files/delete -----------------------------------------------------------

pub async fn delete_entry(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&q.path, &base)?;
    let meta = match std::fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(_) => return Err(ApiError::NotFound("Path not found".to_string())),
    };
    let is_dir = if meta.file_type().is_symlink() {
        std::fs::metadata(&full)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    } else {
        meta.is_dir()
    };
    let result = if is_dir {
        std::fs::remove_dir_all(&full)
    } else {
        std::fs::remove_file(&full)
    };
    result.map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(serde_json::json!({
        "path": full,
        "type": if is_dir { "directory" } else { "file" },
    })))
}
