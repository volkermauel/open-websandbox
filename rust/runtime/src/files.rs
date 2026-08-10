//! `GET|POST /files/*` — the open-terminal filesystem surface.
//!
//! Every handler funnels its caller-supplied path through
//! [`safe_path`](crate::safe_path) first, so traversal/escape attempts come back
//! as HTTP 400 rather than leaking bytes outside the workspace base. Auth is via
//! the [`Authed`] extractor (the per-session key guard).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

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

// --- /ports -----------------------------------------------------------------

pub async fn list_ports(_auth: Authed) -> Json<serde_json::Value> {
    // Restricted runtime: no host-port introspection. Surface an empty list so the
    // UI ports panel renders cleanly (matches open-terminal's restricted fallback).
    Json(serde_json::json!({ "ports": [] }))
}

// --- raw-file response helper (view + download) -----------------------------

/// Stream a file as raw bytes with mime + content-disposition, matching Python's
/// `FileResponse(full, media_type=mime or octet-stream, filename=basename)`.
fn file_response(full: &Path) -> Result<Response, ApiError> {
    let mime = mime_guess::from_path(full).first_or_octet_stream();
    let bytes = std::fs::read(full).map_err(|e| ApiError::Internal(format!("read failed: {e}")))?;
    let filename = full
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    let ct = axum::http::HeaderValue::from_str(mime.as_ref())
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    let cd =
        axum::http::HeaderValue::from_str(format!("attachment; filename=\"{filename}\"").as_str())
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("attachment"));
    let resp = (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, ct),
            (axum::http::header::CONTENT_DISPOSITION, cd),
        ],
        bytes,
    )
        .into_response();
    Ok(resp)
}

// --- /files/view ------------------------------------------------------------

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

// --- /download/{*file_path} -------------------------------------------------

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

pub async fn tool_list(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(file_path): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_impl(&state, &headers, &file_path).await
}

/// `GET /list` + `GET /list/` — list the workspace root. Python's FastAPI route
/// `/list/{file_path:path}` matches the empty path (lists root); axum's
/// `/list/{*file_path}` catch-all requires ≥1 segment, so these explicit routes
/// cover the empty-path case (parity, D11).
pub async fn tool_list_root(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    list_impl(&state, &headers, ".").await
}

async fn list_impl(
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
        match std::fs::metadata(&p) {
            Ok(st) => entries.push(serde_json::json!({
                "name": n,
                "is_dir": st.is_dir(),
                "size": st.len(),
            })),
            Err(_) => continue,
        }
    }
    Ok(Json(
        serde_json::json!({ "path": resolved, "entries": entries }),
    ))
}

// --- /exists/{*file_path} ---------------------------------------------------

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

// --- /files/replace ---------------------------------------------------------

#[derive(Deserialize)]
pub struct ReplacementChunk {
    target: String,
    replacement: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    #[serde(default)]
    allow_multiple: bool,
}

#[derive(Deserialize)]
pub struct ReplaceRequest {
    path: String,
    replacements: Vec<ReplacementChunk>,
}

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
    // Python reads with `errors="replace"` (lossy UTF-8).
    let bytes = std::fs::read(&full).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    for chunk in &req.replacements {
        content = apply_replacement(content, chunk)?;
    }
    let data = content.into_bytes();
    std::fs::write(&full, &data).map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Ok(Json(
        serde_json::json!({ "path": full, "size": data.len() }),
    ))
}

/// Count non-overlapping occurrences of `target` in `segment` (Python str.count).
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

// --- /files/grep -------------------------------------------------------------

#[derive(Deserialize)]
pub struct GrepQuery {
    query: String,
    path: Option<String>,
    regex: Option<bool>,
    case_insensitive: Option<bool>,
    include: Option<String>,
    max_results: Option<usize>,
}

pub async fn grep(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GrepQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let path = q.path.as_deref().unwrap_or(".");
    let resolved = safe_path(path, &base)?;
    if !resolved.exists() {
        return Err(ApiError::NotFound("Search path not found".to_string()));
    }
    let max_results = q.max_results.unwrap_or(50).clamp(1, 500);
    // Python: regex=True compiles the query; regex=False compiles re.escape(query).
    let pattern_src = if q.regex.unwrap_or(true) {
        q.query.clone()
    } else {
        regex::escape(&q.query)
    };
    let re = regex::RegexBuilder::new(&pattern_src)
        .case_insensitive(q.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|e| ApiError::BadRequest(format!("Invalid regex: {e}")))?;
    let mut matches_arr: Vec<serde_json::Value> = Vec::new();
    let include = q.include.as_deref().map(|s| vec![s.to_string()]);
    for fpath in walk_files(&resolved, include.as_deref()) {
        // Python opens with errors="replace"; a read failure (unreadable) is skipped.
        let Ok(fbytes) = std::fs::read(&fpath) else {
            continue;
        };
        let content = String::from_utf8_lossy(&fbytes);
        for (idx, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches_arr.push(serde_json::json!({
                    "file": fpath,
                    "line": idx + 1,
                    "content": line,
                }));
                if matches_arr.len() >= max_results {
                    return Ok(Json(serde_json::json!({
                        "query": q.query,
                        "path": resolved,
                        "matches": matches_arr,
                        "truncated": true,
                    })));
                }
            }
        }
    }
    Ok(Json(serde_json::json!({
        "query": q.query,
        "path": resolved,
        "matches": matches_arr,
        "truncated": false,
    })))
}

/// All regular files under `root` (sorted); optional fnmatch include filter.
/// Mirrors Python `_walk_files`: if `root` is a file, returns `[root]`.
fn walk_files(root: &Path, include: Option<&[String]>) -> Vec<PathBuf> {
    let Ok(meta) = std::fs::metadata(root) else {
        return Vec::new();
    };
    if meta.is_file() {
        return vec![root.to_path_buf()];
    }
    if !meta.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_files(root, include, &mut out);
    out.sort();
    out
}

fn collect_files(dir: &Path, include: Option<&[String]>, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        // os.path.isdir follows symlinks; metadata failure → treat as non-dir.
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            collect_files(&path, include, out);
        } else {
            if let Some(pats) = include {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !pats.iter().any(|p| fnmatch(name, p)) {
                    continue;
                }
            }
            out.push(path);
        }
    }
}

// --- /files/glob -------------------------------------------------------------

#[derive(Deserialize)]
pub struct GlobQuery {
    pattern: String,
    path: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    max_results: Option<usize>,
}

pub async fn glob_search(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GlobQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let path = q.path.as_deref().unwrap_or(".");
    let resolved = safe_path(path, &base)?;
    if !resolved.exists() {
        return Err(ApiError::NotFound("Search directory not found".to_string()));
    }
    let kind = q.kind.as_deref().unwrap_or("any");
    let max_results = q.max_results.unwrap_or(50).clamp(1, 500);
    // Collect all candidates (walked like os.walk), then sort by path; truncated
    // iff total >= max_results (matches Python's append-then-check short-circuit).
    let mut found: Vec<(String, bool, u64, f64)> = Vec::new();
    glob_collect(&resolved, &resolved, &q.pattern, kind, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = found.len() >= max_results;
    let matches_arr: Vec<serde_json::Value> = found
        .into_iter()
        .take(max_results)
        .map(|(path, is_dir, size, modified)| {
            serde_json::json!({
                "path": path,
                "type": if is_dir { "directory" } else { "file" },
                "size": size,
                "modified": modified,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "pattern": q.pattern,
        "path": resolved,
        "matches": matches_arr,
        "truncated": truncated,
    })))
}

/// Walk `dir` like os.walk, pushing matching entries (relpath, is_dir, size, mtime).
fn glob_collect(
    root: &Path,
    dir: &Path,
    pattern: &str,
    kind: &str,
    out: &mut Vec<(String, bool, u64, f64)>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if fnmatch(&rel, pattern) || fnmatch(&name, pattern) {
            if kind == "file" && is_dir {
                // type filter excludes this entry (do not push).
            } else if kind == "directory" && !is_dir {
                // type filter excludes this entry.
            } else if let Ok(st) = std::fs::metadata(&path) {
                // os.stat failure (broken symlink) → skip, matching Python.
                out.push((rel, is_dir, st.len(), modified_secs(&st)));
            }
        }
        if is_dir {
            subdirs.push(path);
        }
    }
    for d in subdirs {
        glob_collect(root, &d, pattern, kind, out);
    }
}

/// Shell-style fnmatch (Python `fnmatch.fnmatch`, case-sensitive on Linux):
/// translates `*`→`.*`, `?`→`.`, `[...]`→char class, anchors the whole string.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let Some(re_src) = fnmatch_translate(pattern) else {
        return false;
    };
    regex::Regex::new(&re_src)
        .map(|re| re.is_match(name))
        .unwrap_or(false)
}

/// Translate a shell glob into an anchored regex, mirroring Python's
/// `fnmatch.translate` (classic form). Returns `^...$`.
fn fnmatch_translate(pat: &str) -> Option<String> {
    let chars: Vec<char> = pat.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut res = String::new();
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => res.push_str(".*"),
            '?' => res.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push_str("\\[");
                } else {
                    let stuff: String = chars[i..j].iter().collect();
                    i = j + 1;
                    let mut s = String::new();
                    let mut sc = stuff.chars();
                    match sc.next() {
                        Some('!') => {
                            s.push('^');
                            s.push_str(sc.as_str());
                        }
                        Some('^') => {
                            s.push_str("\\^");
                            s.push_str(sc.as_str());
                        }
                        other => {
                            if let Some(first) = other {
                                s.push(first);
                            }
                            s.push_str(sc.as_str());
                        }
                    }
                    res.push('[');
                    res.push_str(&s);
                    res.push(']');
                }
            }
            _ => {
                if "\\.+()|^${}".contains(c) {
                    res.push('\\');
                }
                res.push(c);
            }
        }
    }
    Some(format!("^{res}$"))
}

// --- PR-B-5: /files/archive (zip) + /files/upload + /upload (multipart) ------

#[derive(Deserialize)]
pub struct ArchiveRequest {
    pub paths: Vec<String>,
}

#[derive(Deserialize)]
pub struct UploadQuery {
    pub directory: Option<String>,
}

/// Basename of an uploaded filename, never the dir component (defense-in-depth:
/// a multipart field whose `filename` is `../evil` is reduced to `evil` before
/// join, exactly like Python's `os.path.basename`).
fn upload_basename(name: Option<&str>) -> &str {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload")
}

/// `directory` query param for `/files/upload`: the workspace base when absent,
/// empty, or the literal `"null"`; otherwise a safe_path-resolved subdir.
fn upload_target_dir(directory: Option<&str>, base: &Path) -> Result<PathBuf, ApiError> {
    let d = directory.map(str::trim).unwrap_or("");
    if d.is_empty() || d.eq_ignore_ascii_case("null") {
        Ok(base.to_path_buf())
    } else {
        safe_path(d, base)
    }
}

/// `POST /files/upload` — multipart `file` field written to `directory/<basename>`
/// (default the workspace base). The runtime streams the body straight to disk;
/// `X-Workspace-Subdir` selects the base, like every other `/files/*` handler.
pub async fn upload(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let target_dir = upload_target_dir(q.directory.as_deref(), &base)?;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart: {e}")))?
    {
        if field.name() == Some("file") {
            let name = upload_basename(field.file_name()).to_string();
            if !target_dir.is_dir() {
                std::fs::create_dir_all(&target_dir)
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
            }
            let full = target_dir.join(&name);
            // defense-in-depth: target_dir is already under base, but re-check.
            if !full.starts_with(&base) {
                return Err(ApiError::BadRequest("path escapes workspace".into()));
            }
            let mut file = tokio::fs::File::create(&full)
                .await
                .map_err(|e| ApiError::BadRequest(format!("create: {e}")))?;
            let mut size = 0u64;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| ApiError::BadRequest(format!("read: {e}")))?
            {
                size += chunk.len() as u64;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("write: {e}")))?;
            }
            let canon = std::fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
            return Ok(Json(serde_json::json!({ "path": canon, "size": size })));
        }
    }
    Err(ApiError::BadRequest("no 'file' field".into()))
}

/// `POST /upload` — the LLM-tool upload alias (multipart `file` to the workspace
/// base). Returns `{"saved": path, "bytes": n}`, the shape the broker's curated
/// `upload_file` tool resolves against.
pub async fn tool_upload(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart: {e}")))?
    {
        if field.name() == Some("file") {
            let name = upload_basename(field.file_name()).to_string();
            let full = base.join(&name);
            if !full.starts_with(&base) {
                return Err(ApiError::BadRequest("path escapes workspace".into()));
            }
            let mut file = tokio::fs::File::create(&full)
                .await
                .map_err(|e| ApiError::BadRequest(format!("create: {e}")))?;
            let mut bytes = 0u64;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| ApiError::BadRequest(format!("read: {e}")))?
            {
                bytes += chunk.len() as u64;
                file.write_all(&chunk)
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("write: {e}")))?;
            }
            let canon = std::fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
            return Ok(Json(serde_json::json!({ "saved": canon, "bytes": bytes })));
        }
    }
    Err(ApiError::BadRequest("no 'file' field".into()))
}

/// `POST /files/archive` — zip the listed paths and stream the archive back.
/// Dirs recurse (files archived as `<basename>/<rel>`); a single file archives
/// as its basename. `application/zip` + `Content-Disposition: attachment`.
pub async fn archive(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ArchiveRequest>,
) -> Result<Response, ApiError> {
    let base = base_of(&state, &headers)?;
    if req.paths.is_empty() {
        return Err(ApiError::BadRequest("No paths provided".into()));
    }
    let resolved: Vec<PathBuf> = req
        .paths
        .iter()
        .map(|p| {
            let f = safe_path(p, &base)?;
            if !f.exists() {
                return Err(ApiError::NotFound(format!("Path not found: {p}")));
            }
            Ok(f)
        })
        .collect::<Result<_, _>>()?;
    let name = if resolved.len() == 1 {
        resolved[0]
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download")
            .to_string()
    } else {
        "download".to_string()
    };
    let for_task = resolved.clone();
    let buf = tokio::task::spawn_blocking(move || build_zip(&for_task))
        .await
        .map_err(|e| ApiError::Internal(format!("zip join: {e}")))?
        .map_err(|e| ApiError::Internal(format!("zip build: {e}")))?;
    let cd = format!("attachment; filename=\"{name}.zip\"");
    let mut resp = Response::new(Body::from(buf));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/zip".parse().expect("static header"),
    );
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        cd.parse().expect("valid header"),
    );
    Ok(resp)
}

/// Build an in-memory DEFLATE zip of the resolved paths (sorted, deterministic).
fn build_zip(paths: &[PathBuf]) -> std::io::Result<Vec<u8>> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;
    use zip::ZipWriter;

    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for full in paths {
        let arcroot = full
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        if full.is_dir() {
            let mut files: Vec<PathBuf> = Vec::new();
            collect_files_for_zip(full, &mut files)?;
            for fp in files {
                let rel = fp.strip_prefix(full).unwrap_or(&fp);
                let zname = format!("{arcroot}/{}", rel.to_string_lossy());
                zw.start_file(zname, opts)?;
                let mut fh = std::fs::File::open(&fp)?;
                std::io::copy(&mut fh, &mut zw)?;
            }
        } else {
            zw.start_file(&arcroot, opts)?;
            let mut fh = std::fs::File::open(full)?;
            std::io::copy(&mut fh, &mut zw)?;
        }
    }
    Ok(zw.finish()?.into_inner())
}

/// Recursively collect regular files under `root` (depth-first, name-sorted).
fn collect_files_for_zip(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(root)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files_for_zip(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}
