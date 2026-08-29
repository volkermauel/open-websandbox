//! `GET|POST /files/*` — the open-terminal filesystem surface.
//!
//! Every handler funnels its caller-supplied path through
//! [`safe_path`](crate::safe_path) first, so traversal/escape attempts come back
//! as HTTP 400 rather than leaking bytes outside the workspace base. Auth is via
//! the `Authed` extractor (the per-session key guard).

#![forbid(unsafe_code)]

// Filesystem handlers, split by concern (#102 D1):
//   io      read/write/mkdir/move/delete/view/replace + cwd/listing
//   tools   agent tool_* endpoints + ports
//   search  grep + glob
//   archive upload + archive (zip)
// Shared helpers (base_of / modified_secs / file_response) stay here and are
// `pub(super)` for the submodules. The public surface is unchanged.
pub mod archive;
pub mod io;
pub mod search;
pub mod tools;

pub use archive::*;
pub use io::*;
pub use search::*;
pub use tools::*;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::safe_path::request_base;
use crate::state::AppState;

pub(super) fn subdir_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-workspace-subdir")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

pub(super) fn base_of(state: &AppState, headers: &HeaderMap) -> Result<PathBuf, ApiError> {
    request_base(&state.config.workdir, subdir_from(headers))
}

/// Convert a file mtime to seconds-since-epoch (`float(st.st_mtime)`).
pub(super) fn modified_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Honest writability probe mirroring upstream `os.access(path, W_OK)`
/// (open-terminal 0.11.35): read-only mounts, chmod and ownership all factor
/// in. `nix::unistd::access` wraps the syscall safely — no `unsafe` needed.
pub(super) fn is_writable(p: &Path) -> bool {
    nix::unistd::access(p, nix::unistd::AccessFlags::W_OK).is_ok()
}

// --- raw-file response helper (view + download) -----------------------------

/// Stream a file as raw bytes with mime + content-disposition
/// (`FileResponse(full, media_type=mime or octet-stream, filename=basename)`).
pub(super) fn file_response(full: &Path) -> Result<Response, ApiError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authed;
    use crate::auth::SessionKeyStore;
    use crate::config::RuntimeConfig;
    use crate::state::AppState;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::Json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_state(dir: &TempDir) -> AppState {
        AppState::new(
            RuntimeConfig {
                workdir: dir.path().to_path_buf(),
                ..Default::default()
            },
            SessionKeyStore::new(dir.path().join("api-key")),
        )
    }

    /// Regression for #82: `/files/write` must not block its tokio worker. On a
    /// single-worker (`current_thread`) runtime a synchronous `std::fs::write` is
    /// the ONLY thing that worker does for the whole syscall, so at most one write
    /// is ever in flight. Routing the write through `tokio::fs` offloads the
    /// syscall to the blocking pool: each `write_file` future parks at the
    /// `.await`, many are pending simultaneously, and we observe peak in-flight
    /// concurrency strictly greater than 1.
    #[tokio::test(flavor = "current_thread")]
    async fn write_paths_do_not_block_the_runtime() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);

        // Writes currently parked inside `tokio::fs`. With inline `std::fs` on ONE
        // worker this never exceeds 1 (the worker runs each write to completion
        // before polling the next); with `tokio::fs` it climbs as writes pile up
        // on the blocking pool.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..16u32 {
            let state = state.clone();
            let in_flight = in_flight.clone();
            let peak = peak.clone();
            let path = format!("file_{i}.txt");
            let content = "x".repeat(256 * 1024);
            handles.push(tokio::spawn(async move {
                let cur = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                let _ = write_file(
                    Authed,
                    State(state),
                    HeaderMap::new(),
                    Json(WriteRequest { path, content }),
                )
                .await
                .unwrap();
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // `tokio::fs` lets many writes be pending at once on the single worker; an
        // inline blocking `std::fs` would keep this at 1. (16 × 256 KiB writes give
        // a wide window, so this is comfortably > 1 rather than a race.)
        let peak_concurrency = peak.load(Ordering::SeqCst);
        assert!(
            peak_concurrency > 1,
            "writes never overlapped (peak in-flight = {peak_concurrency}); \"
             write_file appears to block the single worker"
        );

        // Correctness: every file landed at the right size.
        for i in 0..16u32 {
            let len = std::fs::metadata(dir.path().join(format!("file_{i}.txt")))
                .expect("file exists")
                .len();
            assert_eq!(len, 256 * 1024, "file {i} has wrong size");
        }
    }

    /// `replace` now reads+writes via `tokio::fs` (issue #82); verify it still
    /// substitutes text correctly end-to-end.
    #[tokio::test]
    async fn replace_uses_async_io() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        tokio::fs::write(dir.path().join("note.md"), "hello world hello")
            .await
            .unwrap();
        let _ = replace(
            Authed,
            State(state),
            HeaderMap::new(),
            Json(ReplaceRequest {
                path: "note.md".to_string(),
                replacements: vec![ReplacementChunk {
                    target: "hello".to_string(),
                    replacement: "bye".to_string(),
                    start_line: None,
                    end_line: None,
                    allow_multiple: true,
                }],
            }),
        )
        .await
        .expect("replace succeeds");
        let after = tokio::fs::read_to_string(dir.path().join("note.md"))
            .await
            .unwrap();
        assert_eq!(after, "bye world bye");
    }
}
