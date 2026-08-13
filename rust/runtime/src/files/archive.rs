//! `files::archive` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::base_of;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::safe_path;
use crate::state::AppState;

// --- PR-B-5: /files/archive (zip) + /files/upload + /upload (multipart) ------

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ArchiveRequest {
    pub paths: Vec<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct UploadQuery {
    pub directory: Option<String>,
}

/// Typed `multipart/form-data` body for the upload endpoints: a single binary
/// `file` part. The runtime consumes the stream directly via
/// `axum::extract::Multipart` (this struct exists only to describe the wire
/// contract for OpenAPI/Scalar — it is never deserialized). The on-disk name
/// is the basename of the part's `Content-Disposition` `filename`; path
/// components are stripped as defense-in-depth (see `upload_basename`).
#[derive(utoipa::ToSchema)]
pub struct FileUpload {
    /// Binary file content (one part named `file`).
    #[schema(format = Binary)]
    #[allow(dead_code)]
    pub file: String,
}

/// Basename of an uploaded filename, never the dir component (defense-in-depth:
/// a multipart field whose `filename` is `../evil` is reduced to `evil` before
/// join, like `os.path.basename`).
fn upload_basename(name: Option<&str>) -> &str {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload")
}

/// `directory` query param for `/files/upload`: the workspace base when absent,
/// empty, or the literal `"null"`; otherwise a safe_path-resolved subdir.
fn upload_target_dir(directory: Option<&str>, base: &Path) -> Result<PathBuf, ApiError> {
    let d = directory.map_or("", str::trim);
    if d.is_empty() || d.eq_ignore_ascii_case("null") {
        Ok(base.to_path_buf())
    } else {
        safe_path(d, base)
    }
}

/// `POST /files/upload` — multipart `file` field written to `directory/<basename>`
/// (default the workspace base). The runtime streams the body straight to disk;
/// `X-Workspace-Subdir` selects the base, like every other `/files/*` handler.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] on a multipart stream error, a disk
/// create/read/write/sync failure, a path that escapes the workspace, or if no
/// `file` part is present.
#[utoipa::path(
    post,
    path = "/files/upload",
    tag = "files",
    params(UploadQuery),
    request_body(content = FileUpload, content_type = "multipart/form-data", description = "multipart `file` field written to `directory`/<basename>"),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Saved path + size", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
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
            file.sync_all()
                .await
                .map_err(|e| ApiError::BadRequest(format!("sync: {e}")))?;
            let canon = std::fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
            return Ok(Json(serde_json::json!({ "path": canon, "size": size })));
        }
    }
    Err(ApiError::BadRequest("no 'file' field".into()))
}

/// `POST /upload` — the LLM-tool upload alias (multipart `file` to the workspace
/// base). Returns `{"saved": path, "bytes": n}`, the shape the broker's curated
/// `upload_file` tool resolves against.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] on a multipart stream error, a disk
/// create/read/write/sync failure, a path that escapes the workspace, or if no
/// `file` part is present.
#[utoipa::path(
    post,
    path = "/upload",
    tag = "tools",
    request_body(content = FileUpload, content_type = "multipart/form-data", description = "multipart `file` field written to the workspace base"),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Saved path + bytes", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
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
            file.sync_all()
                .await
                .map_err(|e| ApiError::BadRequest(format!("sync: {e}")))?;
            let canon = std::fs::canonicalize(&full).unwrap_or_else(|_| full.clone());
            return Ok(Json(serde_json::json!({ "saved": canon, "bytes": bytes })));
        }
    }
    Err(ApiError::BadRequest("no 'file' field".into()))
}

/// `POST /files/archive` — zip the listed paths and stream the archive back.
/// Dirs recurse (files archived as `<basename>/<rel>`); a single file archives
/// as its basename. `application/zip` + `Content-Disposition: attachment`.
///
/// # Panics
///
/// Panics only if the static `application/zip` / `Content-Disposition` header
/// values fail to parse, which cannot happen for these constants.
#[utoipa::path(
    post,
    path = "/files/archive",
    tag = "files",
    request_body = ArchiveRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Zip archive (application/zip, attachment)"),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "A listed path not found", body = shared::ErrorResponse),
        (status = 500, body = shared::ErrorResponse)
    )
)]
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
    entries.sort_by_key(std::fs::DirEntry::file_name);
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
