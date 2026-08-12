//! `files::io` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::{base_of, file_response, modified_secs};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::{open_read, open_write, safe_path};
use crate::state::AppState;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Entry {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    size: u64,
    modified: f64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ListResponse {
    #[schema(value_type = String)]
    dir: PathBuf,
    entries: Vec<Entry>,
}

// --- /files/cwd --------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/files/cwd",
    tag = "files",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Workspace cwd + home", body = serde_json::Value),
        (status = 400, description = "Invalid workspace subdir", body = shared::ErrorResponse),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse)
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CwdRequest {
    pub path: String,
}

#[utoipa::path(
    post,
    path = "/files/cwd",
    tag = "files",
    request_body = CwdRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Resolved cwd", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Directory not found", body = shared::ErrorResponse)
    )
)]
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

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    pub directory: Option<String>,
}

#[utoipa::path(
    get,
    path = "/files/list",
    tag = "files",
    params(ListQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Directory listing", body = ListResponse),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Directory not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
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
    // `os.path.isdir` follows symlinks, so a symlink to a dir is
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

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PathQuery {
    pub path: String,
}

#[utoipa::path(
    get,
    path = "/files/read",
    tag = "files",
    params(PathQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "File content (JSON) or raw image bytes"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
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
        // #99 A5: TOCTOU-safe read — re-open with O_NOFOLLOW + /proc re-resolve
        // so a symlink swapped between safe_path and this read cannot escape.
        let mut file = open_read(&full, &base)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)
            .map_err(|e| ApiError::Internal(format!("read failed: {e}")))?;
        return Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string())],
            bytes,
        )
            .into_response());
    }
    // #99 A5: TOCTOU-safe read (see image branch above).
    let mut file = open_read(&full, &base)?;
    let mut content = String::new();
    std::io::Read::read_to_string(&mut file, &mut content)
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct WriteRequest {
    pub path: String,
    pub content: String,
}

#[utoipa::path(
    post,
    path = "/files/write",
    tag = "files",
    request_body = WriteRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Bytes written", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
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
            // Non-blocking I/O (issue #82): `tokio::fs` runs the syscall on the
            // blocking pool so this async handler never stalls its tokio worker.
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ApiError::BadRequest(format!("{e}")))?;
        }
    }
    // Non-blocking + TOCTOU-safe write (#82 + #99 A5): the confined open (open_write,
    // O_NOFOLLOW + /proc re-resolve, create+truncate) and the data write both run on
    // the blocking pool so the single tokio worker is never held, and the write is
    // fully complete before the handler returns.
    let size = data.len();
    let full_for_write = full.clone();
    tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let mut file = open_write(&full_for_write, &base, true, true)?;
        std::io::Write::write_all(&mut file, &data)
            .map_err(|e| ApiError::BadRequest(format!("{e}")))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("write join failed: {e}")))??;
    Ok(Json(serde_json::json!({ "path": full, "size": size })))
}

// --- /files/mkdir ------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/files/mkdir",
    tag = "files",
    request_body = PathBody,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Created path", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
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

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PathBody {
    pub path: String,
}

// --- /files/move -------------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct MoveRequest {
    pub source: String,
    pub destination: String,
}

#[utoipa::path(
    post,
    path = "/files/move",
    tag = "files",
    request_body = MoveRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Moved source→destination", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Source not found", body = shared::ErrorResponse),
        (status = 409, description = "Destination already exists", body = shared::ErrorResponse)
    )
)]
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

#[utoipa::path(
    delete,
    path = "/files/delete",
    tag = "files",
    params(PathQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Deleted entry", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Path not found", body = shared::ErrorResponse)
    )
)]
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

// --- /files/view ------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/files/view",
    tag = "files",
    params(PathQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Raw file bytes (content-type/disposition)"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
pub async fn view_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Response, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&q.path, &base)?;
    if !full.is_file() {
        return Err(ApiError::NotFound("File not found".to_string()));
    }
    file_response(&full)
}

// --- /files/replace ---------------------------------------------------------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReplacementChunk {
    pub target: String,
    pub replacement: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    #[serde(default)]
    pub allow_multiple: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReplaceRequest {
    pub path: String,
    pub replacements: Vec<ReplacementChunk>,
}

#[utoipa::path(
    post,
    path = "/files/replace",
    tag = "files",
    request_body = ReplaceRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Bytes written", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse)
    )
)]
pub async fn replace(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ReplaceRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&req.path, &base)?;
    if !full.is_file() {
        return Err(ApiError::NotFound("File not found".to_string()));
    }
    // Lossy UTF-8 read (errors="replace").
    // Non-blocking read (issue #82): was `std::fs::read`.
    let bytes = tokio::fs::read(&full)
        .await
        .map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    for chunk in &req.replacements {
        content = apply_replacement(content, chunk)?;
    }
    let data = content.into_bytes();
    // Non-blocking write (issue #82): was `std::fs::write`.
    tokio::fs::write(&full, &data)
        .await
        .map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(
        serde_json::json!({ "path": full, "size": data.len() }),
    ))
}

/// Count non-overlapping occurrences of `target` in `segment` (`str.count`).
fn count_occurrences(segment: &str, target: &str) -> usize {
    if target.is_empty() {
        return 0;
    }
    segment.matches(target).count()
}

/// Replace `target` in `segment` per chunk flags (mirrors `_replace_target`).
fn replace_target(segment: &str, chunk: &ReplacementChunk) -> Result<String, ApiError> {
    let count = count_occurrences(segment, &chunk.target);
    if count == 0 {
        return Err(ApiError::BadRequest(format!(
            "Target string not found: {}",
            chunk.target
        )));
    }
    if count > 1 && !chunk.allow_multiple {
        return Err(ApiError::BadRequest(format!(
            "Found {count} occurrences of target but allow_multiple is false"
        )));
    }
    Ok(if chunk.allow_multiple {
        segment.replace(&chunk.target, &chunk.replacement)
    } else {
        segment.replacen(&chunk.target, &chunk.replacement, 1)
    })
}

/// Apply one replacement chunk, optionally line-scoped (mirrors `_apply_replacement`).
fn apply_replacement(content: String, chunk: &ReplacementChunk) -> Result<String, ApiError> {
    if chunk.start_line.is_none() && chunk.end_line.is_none() {
        return replace_target(&content, chunk);
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let start = chunk.start_line.unwrap_or(1).saturating_sub(1);
    let end = match chunk.end_line {
        None => lines.len(),
        Some(e) => std::cmp::min(lines.len(), e),
    };
    if start >= end {
        return Ok(content);
    }
    let segment = lines[start..end].join("\n");
    let new_segment = replace_target(&segment, chunk)?;
    // Rebuild: lines[..start] + new_segment split + lines[end..]
    let mut rebuilt: Vec<&str> = lines[..start].to_vec();
    rebuilt.extend(new_segment.split('\n'));
    rebuilt.extend_from_slice(&lines[end..]);
    Ok(rebuilt.join("\n"))
}
