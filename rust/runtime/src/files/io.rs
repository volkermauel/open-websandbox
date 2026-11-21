//! `files::io` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::{base_of, file_response, modified_secs};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::{open_read, open_write, safe_path};
use crate::state::AppState;

/// One entry in a directory listing (name, type, size, modified time, writability).
#[derive(Serialize, utoipa::ToSchema)]
pub struct Entry {
    name: String,
    #[serde(rename = "type")]
    kind: &'static str,
    size: u64,
    modified: f64,
    /// Birth time in seconds since the epoch (open-terminal 0.17.x field;
    /// `null` when the filesystem cannot report one).
    created: Option<f64>,
    /// Whether the entry is writable (0.11.35 open-terminal contract: `access(W_OK)`).
    writable: bool,
}

/// A directory listing: the resolved directory, its writability, and sorted entries.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ListResponse {
    #[schema(value_type = String)]
    dir: PathBuf,
    /// Whether the listed directory itself is writable (0.11.35).
    writable: bool,
    entries: Vec<Entry>,
}

// --- /files/cwd --------------------------------------------------------------

/// Return the effective workspace cwd — == home == file-browser `root`
/// (upstream `get_file_browser_root`; #179 FileNav parity) — for the session.
///
/// # Errors
///
/// Returns [`ApiError`] (via `base_of`) if `X-Workspace-Subdir` is invalid.
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
        "root": base,
    })))
}

/// Request body for `POST /files/cwd`: the workspace-relative directory to switch to.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CwdRequest {
    /// Workspace-relative directory to make the effective cwd.
    pub path: String,
}

/// Set the session cwd to a workspace-relative directory.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace and
/// [`ApiError::NotFound`] if it is missing or not a directory.
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

/// Query parameters for `GET /files/list`: an optional directory (defaults to `.`).
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Optional workspace-relative directory to list (defaults to `.`).
    pub directory: Option<String>,
}

/// List the entries of a workspace-relative directory.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace,
/// [`ApiError::NotFound`] if it is missing or not a directory, and
/// [`ApiError::Internal`] if the directory cannot be read.
#[utoipa::path(
    get,
    path = "/files/list",
    operation_id = "list_files",
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
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        if let Some(entry) = entry_for(&resolved.join(&name)) {
            entries.push(entry);
        }
    }
    fill_created_via_stat(&resolved, &mut entries);
    Ok(Json(ListResponse {
        dir: resolved.clone(),
        writable: super::is_writable(&resolved),
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
        std::fs::metadata(p).is_ok_and(|m| m.is_dir())
    } else {
        meta.is_dir()
    };
    Some(Entry {
        name,
        kind: if is_dir { "directory" } else { "file" },
        size: meta.len(),
        modified: modified_secs(&meta),
        // Birth time from the FOLLOWED stat (upstream listdir uses `os.stat`).
        // `None` when the platform/FS cannot report it — the GNU-stat batch
        // fallback in `fill_created_via_stat` runs after the listing.
        created: std::fs::metadata(p)
            .ok()
            .and_then(|m| m.created().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64()),
        writable: super::is_writable(p),
    })
}

/// Backfill `created` for entries whose filesystem reported no birth time via
/// `statx` — the upstream fallback (GNU `stat -L --printf='%n\0%.9W\0'`,
/// batched 256 names per invocation, `LC_ALL=C`, relative names under the
/// listed directory). `%.9W` yields `0` when the FS has no birth time, which
/// upstream maps to `null` (`float("0") or None`) — mirrored exactly.
fn fill_created_via_stat(resolved: &Path, entries: &mut [Entry]) {
    if entries.iter().all(|e| e.created.is_some()) {
        return;
    }
    for chunk in entries.chunks_mut(256) {
        let missing: Vec<&str> = chunk
            .iter()
            .filter(|e| e.created.is_none())
            .map(|e| e.name.as_str())
            .collect();
        if missing.is_empty() {
            continue;
        }
        let output = std::process::Command::new("stat")
            .arg("-L")
            .arg(r"--printf=%n\0%.9W\0")
            .args(&missing)
            .current_dir(resolved)
            .env("LC_ALL", "C")
            .output();
        let Ok(out) = output else { return };
        if !out.status.success() && out.stdout.is_empty() {
            return;
        }
        let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        let created: std::collections::HashMap<&[u8], f64> = fields
            .chunks(2)
            .filter_map(|pair| match pair {
                [name, value] => std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|v| (*name, v)),
                _ => None,
            })
            .collect();
        for entry in chunk.iter_mut() {
            if entry.created.is_none() {
                entry.created = created
                    .get(entry.name.as_bytes())
                    .copied()
                    .filter(|v| *v > 0.0);
            }
        }
    }
}

// --- /files/read -------------------------------------------------------------

/// Query parameter naming a single workspace-relative path.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PathQuery {
    /// Workspace-relative path to operate on.
    pub path: String,
}

/// Query parameters for `GET /files/read` (open-terminal 0.2.7).
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReadQuery {
    /// Workspace-relative path to operate on.
    pub path: String,
    /// Optional 1-indexed inclusive first line.
    pub start_line: Option<usize>,
    /// Optional 1-indexed inclusive last line.
    pub end_line: Option<usize>,
}

/// Read a workspace file as JSON text, or raw bytes for an image.
///
/// Mirrors the open-terminal 0.2.7 contract: optional 1-indexed inclusive
/// `start_line`/`end_line` slicing for text; images return raw bytes; any
/// other non-UTF-8 binary returns 415 (Office/text extraction deferred —
/// see docs/compatibility.md).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace or a
/// line range is zero, [`ApiError::NotFound`] if it is missing or not a regular
/// file, [`ApiError::UnsupportedMediaType`] for non-image binaries, and
/// [`ApiError::Internal`] on a read failure.
#[utoipa::path(
    get,
    path = "/files/read",
    tag = "files",
    params(ReadQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "File content (JSON) or raw image bytes"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse),
        (status = 415, description = "Unsupported binary file type", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
pub async fn read_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ReadQuery>,
) -> Result<Response, ApiError> {
    if q.start_line == Some(0) || q.end_line == Some(0) {
        return Err(ApiError::BadRequest(
            "start_line/end_line are 1-indexed".to_string(),
        ));
    }
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
    std::io::Read::read_to_string(&mut file, &mut content).map_err(|e| {
        // InvalidData = non-UTF-8: mirror open-terminal's 415 for every
        // non-image binary (its Office-extraction branch is deferred).
        if e.kind() == std::io::ErrorKind::InvalidData {
            let len = meta.len();
            ApiError::UnsupportedMediaType(format!(
                "Unsupported binary file type: {mime} ({len} bytes)"
            ))
        } else {
            ApiError::Internal(format!("read failed: {e}"))
        }
    })?;
    // keepends line slicing (`splitlines(keepends=True)` parity for \n/\r\n).
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let start = q.start_line.unwrap_or(1).saturating_sub(1);
    let end = q.end_line.unwrap_or(lines.len()).min(lines.len());
    let sliced: String = if start >= end {
        String::new()
    } else {
        lines[start..end].concat()
    };
    Ok(Json(serde_json::json!({
        "path": full,
        "total_lines": lines.len(),
        "content": sliced,
    }))
    .into_response())
}
// --- /files/serve ------------------------------------------------------------

/// Serve a workspace file inline with its guessed content type.
///
/// Path-parameter alias of the view semantics introduced by open-terminal
/// 0.11.34 so OWUI FileNav iframes resolve relative assets. Unlike
/// [`view_file`] there is **no** `Content-Disposition: attachment` header —
/// browsers must render the bytes inline.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace and
/// [`ApiError::NotFound`] if it is missing or not a regular file.
#[utoipa::path(
    get,
    path = "/files/serve/{file_path}",
    tag = "files",
    params(
        ("file_path" = String, Path, description = "Workspace-relative file path"),
    ),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Raw file bytes with guessed content type"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "File not found", body = shared::ErrorResponse)
    )
)]
pub async fn serve_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(file_path): axum::extract::Path<String>,
) -> Result<Response, ApiError> {
    let base = base_of(&state, &headers)?;
    let full = safe_path(&file_path, &base)?;
    if !full.is_file() {
        return Err(ApiError::NotFound("File not found".to_string()));
    }
    let mime = mime_guess::from_path(&full).first_or_octet_stream();
    // #99 A5: TOCTOU-safe read — O_NOFOLLOW + /proc re-resolve.
    let mut file = open_read(&full, &base)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)
        .map_err(|e| ApiError::Internal(format!("read failed: {e}")))?;
    let ct = axum::http::HeaderValue::from_str(mime.as_ref())
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, ct)],
        bytes,
    )
        .into_response())
}

// --- /files/display ---------------------------------------------------------

/// Signal that a file should be shown to the user (upstream 0.2.9 contract).
///
/// This is a **signaling** endpoint, not a byte-serving one: upstream
/// `display_file` (open_terminal/main.py @ v0.12.3) resolves the path and
/// reports `exists`; the consuming client opens its own viewer. A missing
/// file is a successful `exists: false` response — only an invalid path
/// errors. Divergence: ours confines the resolved path to the workspace
/// (400 on escape) like every other file endpoint; upstream single-user
/// mode resolves arbitrary host paths.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace.
#[utoipa::path(
    get,
    path = "/files/display",
    tag = "files",
    params(PathQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Resolved path + exists flag (show-file signal)", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn display_file(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let resolved = safe_path(&q.path, &base)?;
    let exists = resolved.is_file();
    Ok(Json(
        serde_json::json!({ "path": resolved, "exists": exists }),
    ))
}

// --- /files/write ------------------------------------------------------------

/// Request body for `POST /files/write`: overwrite a file with the given content.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct WriteRequest {
    /// Workspace-relative file to overwrite (parents created as needed).
    pub path: String,
    /// Full text content to write to the file.
    pub content: String,
}

/// Overwrite a workspace file (creating parents as needed).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace or the
/// create/open/write fails, and [`ApiError::Internal`] if the blocking write
/// task joins with an error.
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
    // `created` bool + diff base (open-terminal 0.17.x contract): whether the
    // target existed BEFORE the write, and its previous TEXT (strict UTF-8 —
    // binary files yield no patch, like upstream's `fs.read_text` catching
    // UnicodeDecodeError).
    let created = !full.exists();
    let previous: Option<String> = if created {
        None
    } else {
        tokio::fs::read(&full)
            .await
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    };
    let new_text = req.content;
    let data = new_text.clone().into_bytes();
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
    let mut result = serde_json::json!({ "path": full, "size": size, "created": created });
    // Edit-diff card field (OWUI renders `patch` as added/removed lines). A new
    // file's diff is the content itself, which the caller already has — the
    // patch is computed only for overwrites, exactly like upstream.
    if let Some(prev) = previous.as_deref() {
        let display = full.display().to_string();
        if let Some(patch) = edit_patch(&display, prev, &new_text) {
            result["patch"] = serde_json::json!(patch);
        }
    }
    Ok(Json(result))
}

/// Port of upstream `utils/fs.edit_patch` (open-terminal 0.17.20): a unified
/// diff of `old`→`new` (`difflib.unified_diff`, context 3, `lineterm=""`),
/// returned as `None` when the diff body would exceed the combined input size
/// (`changes <= len(old) + len(new)` in characters) — OWUI then falls back to
/// rendering the text itself. Identical inputs yield the empty string (an
/// empty `difflib` generator), not a header-only stub.
fn edit_patch(path: &str, old: &str, new: &str) -> Option<String> {
    let diff = similar::TextDiff::from_lines(old, new);
    // No hunks (identical inputs) → difflib's generator is empty → "".
    if diff.grouped_ops(3).is_empty() {
        return Some(String::new());
    }
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    let lines: Vec<&str> = unified.lines().collect();
    // Upstream skips the two `---`/`+++` header lines; Python `len()` counts
    // characters, not bytes.
    let changes: usize = lines
        .iter()
        .skip(2)
        .filter(|line| !line.starts_with("@@"))
        .map(|line| line.chars().count() + 1)
        .sum();
    let budget = old.chars().count() + new.chars().count();
    (changes <= budget).then(|| lines.join("\n"))
}

// --- /files/mkdir ------------------------------------------------------------

/// Create a workspace directory (and missing parents).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace or the
/// directory cannot be created.
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

/// Request body carrying a single workspace-relative path (`mkdir`, etc.).
#[derive(Deserialize, utoipa::ToSchema)]
pub struct PathBody {
    /// Workspace-relative path to operate on.
    pub path: String,
}

// --- /files/move -------------------------------------------------------------

/// Request body for `POST /files/move`: rename `source` to `destination`.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct MoveRequest {
    /// Workspace-relative path of the entry to move.
    pub source: String,
    /// Workspace-relative destination path (must not already exist).
    pub destination: String,
}

/// Move/rename a workspace entry (source -> destination).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if either path escapes the workspace or
/// the rename fails, [`ApiError::NotFound`] if the source is missing, and
/// [`ApiError::Conflict`] if the destination already exists.
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

/// Delete a workspace file or directory (recursively).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace or the
/// removal fails, and [`ApiError::NotFound`] if the path is missing.
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
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return Err(ApiError::NotFound("Path not found".to_string()));
    };
    let is_dir = if meta.file_type().is_symlink() {
        std::fs::metadata(&full).is_ok_and(|m| m.is_dir())
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

/// Stream a workspace file's raw bytes (content-type + disposition).
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace,
/// [`ApiError::NotFound`] if it is missing or not a regular file, and
/// [`ApiError::Internal`] on a read failure.
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

/// One find/replace chunk applied to a workspace file.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReplacementChunk {
    /// Literal string to search for.
    pub target: String,
    /// Text substituted in place of `target`.
    pub replacement: String,
    /// Optional 1-based start line for a line-scoped replacement.
    pub start_line: Option<usize>,
    /// Optional inclusive 1-based end line for a line-scoped replacement.
    pub end_line: Option<usize>,
    /// When `true`, replace every occurrence; otherwise require exactly one match.
    #[serde(default)]
    pub allow_multiple: bool,
}

/// Request body for `POST /files/replace`: apply find/replace chunks to one file.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct ReplaceRequest {
    /// Workspace-relative file to edit.
    pub path: String,
    /// Ordered find/replace chunks applied to the file.
    pub replacements: Vec<ReplacementChunk>,
}

/// Apply one or more find/replace chunks to a workspace file.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace, the
/// read/write fails, or a replacement target is missing/ambiguous, and
/// [`ApiError::NotFound`] if the file is missing.
#[utoipa::path(
    post,
    path = "/files/replace",
    operation_id = "replace_file_content",
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
    let original = String::from_utf8_lossy(&bytes).into_owned();
    let mut content = original.clone();
    for chunk in &req.replacements {
        content = apply_replacement(content, chunk)?;
    }
    // Edit-diff card field (open-terminal 0.17.20): the whole-edit diff, capped
    // like upstream (`None` over the size budget ⇒ key omitted). Computed
    // before the move into bytes.
    let display = full.display().to_string();
    let patch = edit_patch(&display, &original, &content);
    let data = content.into_bytes();
    // Non-blocking write (issue #82): was `std::fs::write`.
    tokio::fs::write(&full, &data)
        .await
        .map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    let mut result = serde_json::json!({ "path": full, "size": data.len() });
    if let Some(patch) = patch {
        result["patch"] = serde_json::json!(patch);
    }
    Ok(Json(result))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_patch_renders_a_unified_diff() {
        // A one-line full replacement of equal-length text exceeds upstream's
        // cap (24 > 12+10) — use a small in-budget edit instead.
        let patch = edit_patch("note.md", "line1\nline2\nline3\n", "line1\nXX\nline3\n");
        let patch = patch.expect("small diff under the size budget");
        assert!(patch.starts_with("--- note.md\n+++ note.md\n"), "{patch:?}");
        assert!(patch.contains("@@ -1,3 +1,3 @@"), "{patch:?}");
        assert!(patch.contains("-line2\n+XX"), "{patch:?}");
    }

    #[test]
    fn edit_patch_identical_input_yields_empty_string() {
        // difflib's generator is empty for identical inputs — the patch key is
        // "" (present), not a header-only stub and not omitted.
        assert_eq!(edit_patch("f.txt", "same\n", "same\n"), Some(String::new()));
    }

    #[test]
    fn edit_patch_none_over_the_size_budget() {
        // Upstream cap: the diff body must not exceed len(old)+len(new) chars —
        // a full rewrite triples the text, blowing the budget.
        let old = "line\n".repeat(50);
        let new = "replaced\n".repeat(50);
        assert_eq!(edit_patch("big.txt", &old, &new), None);
        // And a small tweak of a large file stays under it.
        let tweaked = format!("{old}extra\n");
        assert!(edit_patch("big.txt", &old, &tweaked).is_some());
    }

    #[test]
    fn created_field_serializes_null_when_unknown() {
        // The wire contract always carries the key (open-terminal emits
        // `"created": null` when the FS has no birth time).
        let entry = Entry {
            name: "x".into(),
            kind: "file",
            size: 1,
            modified: 0.0,
            created: None,
            writable: true,
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert!(v.get("created").is_some_and(serde_json::Value::is_null));
    }
}
