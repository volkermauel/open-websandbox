//! `GET /snapshot` + `PUT /restore` — S3-tiered workspace offload/restore (#52).
//!
//! The broker is the sole S3 client; the runtime only streams a zstd-compressed
//! tar of the whole workspace off (`GET /snapshot`) and back on (`PUT /restore`)
//! over the per-session key, exactly like the Python runtime. Both spawn the
//! **native `tar` + `zstd` binaries** (decision D6 — no Rust tar/zstd crates) and
//! pipe their stdio between stages and to/from the HTTP body with
//! [`tokio::process::Command`] + `tokio::io::copy`.
//!
//! The snapshot pipeline is `find . -mindepth 1 -print0 | tar --null
//! --no-recursion -cf - -T - | zstd -3 -q`, run as THREE separate processes wired
//! by explicit pipes (find→tar→zstd). Crucially it never emits a leading `.`
//! entry, so restoring into a root-owned emptyDir mountpoint doesn't make tar
//! try to chown/chmod the mountpoint (which only root may do) and fail.
//!
//! Size safety (D9 fail-on-exceed):
//! * `/snapshot` pre-checks the apparent workspace size against
//!   [`RuntimeConfig::max_workspace_bytes`] and returns **413 before streaming**;
//! * `/restore` counts the COMPRESSED incoming bytes and aborts with **413** the
//!   instant the running total exceeds the cap (it never buffers the whole body).
//!
//! A non-zero pipeline exit on restore is a **500**; on snapshot it is only
//! logged (the 200/streaming response is already committed by then).

#![forbid(unsafe_code)]

use std::convert::Infallible;
use std::path::Path;
use std::process::Stdio;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use http_body_util::channel::Channel;
use http_body_util::BodyExt;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::request_base;
use crate::state::AppState;

/// Streaming read chunk (matches the Python `proc.stdout.read(1 << 20)`).
const CHUNK: usize = 1 << 20;
/// Bounded frames in flight between the producer task and the response body.
const BODY_CHANNEL_DEPTH: usize = 8;

#[derive(Serialize, utoipa::ToSchema)]
pub struct RestoreResponse {
    restored: bool,
    bytes: u64,
}

fn spawn_err(e: std::io::Error) -> ApiError {
    ApiError::Internal(format!("failed to spawn tar/zstd pipeline: {e}"))
}

/// Apply the same hardened spawn shape as `execute.rs`: own process group (so a
/// stray tree can be reaped) + `kill_on_drop` (so a dropped handler/stream cannot
/// leak an orphaned tar/zstd into the sandbox).
fn hardened(cmd: &mut Command) -> &mut Command {
    cmd.process_group(0).kill_on_drop(true)
}

// ---------------------------------------------------------------------------
// GET /snapshot
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/snapshot",
    tag = "snapshot",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Workspace as a zstd-compressed tar stream (application/zstd)"),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse),
        (status = 413, description = "Workspace exceeds MAX_WORKSPACE_BYTES", body = shared::ErrorResponse),
        (status = 500, description = "Pipeline failure", body = shared::ErrorResponse)
    )
)]
pub async fn snapshot(_auth: Authed, State(state): State<AppState>) -> Result<Response, ApiError> {
    let base = request_base(&state.config.workdir, None)?;

    // Pre-check: refuse BEFORE opening any pipe (D9 fail-on-exceed).
    if workspace_size(&base) > state.config.max_workspace_bytes {
        return Err(ApiError::PayloadTooLarge(
            "workspace exceeds MAX_WORKSPACE_BYTES".to_string(),
        ));
    }

    // find . -mindepth 1 -print0   (cwd = base; no leading '.' entry)
    let mut find = Command::new("find");
    hardened(&mut find)
        .current_dir(&base)
        .args([".", "-mindepth", "1", "-print0"])
        // `find` reads no stdin; stderr discarded (quiet on success, rc is the
        // signal — discarding avoids any pipe-buffer deadlock during streaming).
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut find_c = find.spawn().map_err(spawn_err)?;

    // tar --null --no-recursion -cf - -T -   (stdin <- find, stdout -> zstd)
    let mut tar = Command::new("tar");
    hardened(&mut tar)
        // Same `cd base` as find: it stats the listed `./...` paths.
        .current_dir(&base)
        .args(["--null", "--no-recursion", "-cf", "-", "-T", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut tar_c = tar.spawn().map_err(spawn_err)?;

    // zstd -3 -q   (stdin <- tar, stdout -> HTTP body)
    let mut zstd = Command::new("zstd");
    hardened(&mut zstd)
        .current_dir(&base)
        .args(["-3", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut zstd_c = zstd.spawn().map_err(spawn_err)?;

    // Wire the inter-stage pipes: find.stdout -> tar.stdin,
    // tar.stdout -> zstd.stdin. Each copy task owns both pipe ends; dropping the
    // writer on reader-EOF cascades EOF downstream (the natural shell-pipe close).
    let mut find_out = find_c.stdout.take().expect("find stdout piped");
    let mut tar_in = tar_c.stdin.take().expect("tar stdin piped");
    let mut tar_out = tar_c.stdout.take().expect("tar stdout piped");
    let mut zstd_in = zstd_c.stdin.take().expect("zstd stdin piped");
    let mut zstd_out = zstd_c.stdout.take().expect("zstd stdout piped");
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut find_out, &mut tar_in).await;
    });
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tar_out, &mut zstd_in).await;
    });

    // Stream zstd's stdout into the response body from a producer task. The
    // `Channel` is a native body (no manual `Stream` impl): `send_data` resolves
    // to `Err` once the response body is dropped (client gone), at which point we
    // stop reading and reap the pipeline.
    let (mut sender, body_rx) = Channel::<Bytes, Infallible>::new(BODY_CHANNEL_DEPTH);
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match zstd_out.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    if sender.send_data(chunk).await.is_err() {
                        // Body receiver gone (client disconnect) — stop draining.
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        reap_pipeline([zstd_c, tar_c, find_c]).await;
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zstd")
        .header(
            header::CONTENT_DISPOSITION,
            r#"attachment; filename="workspace.tar.zst""#,
        )
        .body(Body::new(body_rx))
        .map_err(|e| ApiError::Internal(format!("snapshot response build failed: {e}")))
}

// ---------------------------------------------------------------------------
// PUT /restore
// ---------------------------------------------------------------------------

#[utoipa::path(
    put,
    path = "/restore",
    tag = "snapshot",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Workspace restored", body = RestoreResponse),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse),
        (status = 413, description = "Restore stream exceeds MAX_WORKSPACE_BYTES", body = shared::ErrorResponse),
        (status = 500, description = "Pipeline failure", body = shared::ErrorResponse)
    )
)]
pub async fn restore(
    _auth: Authed,
    State(state): State<AppState>,
    body: Body,
) -> Result<Json<RestoreResponse>, ApiError> {
    let base = request_base(&state.config.workdir, None)?;
    let cap = state.config.max_workspace_bytes;

    // zstd -d -q   (stdin <- HTTP body, stdout -> tar.stdin)
    let mut zstd = Command::new("zstd");
    hardened(&mut zstd)
        .args(["-d", "-q"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut zstd_c = zstd.spawn().map_err(spawn_err)?;

    // tar -xf - -C base   (stdin <- zstd.stdout)
    let mut tar = Command::new("tar");
    hardened(&mut tar)
        .args(["-xf", "-", "-C"])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut tar_c = tar.spawn().map_err(spawn_err)?;

    let mut zstd_in = zstd_c.stdin.take().expect("zstd stdin piped");
    let mut zstd_out = zstd_c.stdout.take().expect("zstd stdout piped");
    let mut tar_in = tar_c.stdin.take().expect("tar stdin piped");
    let zstd_err = zstd_c.stderr.take().expect("zstd stderr piped");
    let tar_err = tar_c.stderr.take().expect("tar stderr piped");

    // Drain zstd.stdout -> tar.stdin concurrently with feeding the body, and
    // capture both stderrs so a 500 can carry the failing stage's message
    // (mirrors the Python `err[:200]` detail). Draining also prevents a
    // pipe-buffer deadlock if a stage logs to stderr.
    let copy_task = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut zstd_out, &mut tar_in).await;
    });
    let zstd_err_task = tokio::spawn(read_all(zstd_err));
    let tar_err_task = tokio::spawn(read_all(tar_err));

    // Stream the request body into zstd's stdin, counting COMPRESSED bytes and
    // aborting the instant the running total exceeds the cap (Python breaks
    // before writing the chunk that crosses the limit).
    let mut body = body;
    let mut received: u64 = 0;
    let mut exceeded = false;
    let mut body_err: Option<String> = None;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                body_err = Some(format!("{e}"));
                break;
            }
        };
        let chunk = match frame.into_data() {
            Ok(b) => b,
            Err(_) => continue, // trailers frame — ignore
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > cap {
            exceeded = true;
            break;
        }
        if zstd_in.write_all(&chunk).await.is_err() {
            // Pipeline exited early on bad input; reported via the rc path below.
            break;
        }
    }
    // Dropping stdin sends EOF so zstd (then tar) terminate naturally.
    drop(zstd_in);

    if exceeded {
        let _ = reap_pipeline([zstd_c, tar_c]).await;
        return Err(ApiError::PayloadTooLarge(format!(
            "restore stream exceeds MAX_WORKSPACE_BYTES ({cap})"
        )));
    }

    let zstatus = zstd_c.wait().await;
    let tstatus = tar_c.wait().await;
    let _ = copy_task.await;
    let zerr = zstd_err_task.await.unwrap_or_default();
    let terr = tar_err_task.await.unwrap_or_default();

    if let Some(msg) = body_err {
        return Err(ApiError::Internal(format!(
            "restore stream read failed: {msg}"
        )));
    }

    let zcode = zstatus.as_ref().ok().and_then(|s| s.code());
    let tcode = tstatus.as_ref().ok().and_then(|s| s.code());
    let pipeline_ok = || zstatus.is_ok() && tstatus.is_ok() && zcode == Some(0) && tcode == Some(0);
    if !pipeline_ok() {
        let detail = first_err(&zerr, &terr);
        tracing::error!(
            zcode = ?zcode,
            tcode = ?tcode,
            base = %base.display(),
            "restore pipeline failed"
        );
        return Err(ApiError::Internal(format!(
            "restore pipeline failed (zstd rc={zcode:?}, tar rc={tcode:?}): {detail}"
        )));
    }

    tracing::info!(base = %base.display(), received, "restore into workspace");
    Ok(Json(RestoreResponse {
        restored: true,
        bytes: received,
    }))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Apparent bytes of everything under `base` (snapshot pre-check).
///
/// Walks with `symlink_metadata` so a symlink counts its OWN (link) size, never
/// its target's, and never recurses INTO a symlinked directory (matches the
/// `os.walk(followlinks=False)` shape). Files that vanish mid-walk are skipped,
/// mirroring the Python `except OSError: continue`.
fn workspace_size(base: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                // Symlink (to file or dir): count the link size, never follow.
                *total += meta.len();
            } else if meta.is_dir() {
                walk(&entry.path(), total);
            } else {
                *total += meta.len();
            }
        }
    }
    let mut total = 0u64;
    walk(base, &mut total);
    total
}

/// Kill (if still running) then wait every child, returning nothing — the
/// individual exit codes are only consulted by the caller where they matter.
///
/// Kill-before-wait matters on the disconnect/abort paths: a stage can be
/// blocked writing into a pipe nobody is draining, in which case a bare `wait`
/// would hang. `kill` on an already-exited child is a benign no-op, so this is
/// safe on the clean-EOF path too.
async fn reap_pipeline(children: impl IntoIterator<Item = Child>) {
    let mut kids: Vec<Child> = children.into_iter().collect();
    for c in &mut kids {
        // `Child::kill` is async and returns a future; awaiting it actually
        // sends SIGKILL (a bare `let _ = c.kill()` would drop the future
        // un-run). Err (ESRCH) on an already-exited child is ignored.
        let _ = c.kill().await;
    }
    let mut any_nonzero = false;
    for c in &mut kids {
        match c.wait().await {
            Ok(s) if s.code() != Some(0) => any_nonzero = true,
            Err(_) => any_nonzero = true,
            _ => {}
        }
    }
    if any_nonzero {
        tracing::warn!("snapshot/restore pipeline exited non-zero");
    }
}

/// Read a piped stream to EOF into a buffer (best-effort; errors ignored).
async fn read_all<R: AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    buf
}

/// First-line diagnostic from the (possibly two) captured stderr streams,
/// truncated to ~200 bytes (matches the Python `err[:200]` 500-detail shape).
fn first_err(a: &[u8], b: &[u8]) -> String {
    let pick = |x: &[u8]| String::from_utf8_lossy(x).trim().to_string();
    let mut combined = pick(a);
    if combined.is_empty() {
        combined = pick(b);
    } else {
        let pb = pick(b);
        if !pb.is_empty() {
            combined.push('\n');
            combined.push_str(&pb);
        }
    }
    combined.chars().take(200).collect()
}
