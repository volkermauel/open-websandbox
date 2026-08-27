//! `GET /snapshot` + `PUT /restore` — S3-tiered workspace offload/restore (#52).
//!
//! The broker is the sole S3 client; the runtime only streams a zstd-compressed
//! tar of the whole workspace off (`GET /snapshot`) and back on (`PUT /restore`)
//! over the per-session key.
//!
//! #94: the former `find`/`tar`/`zstd` **CLI child processes** (and all their
//! plumbing — `hardened()` lockdown, inter-stage pipe copying, stderr capture,
//! `reap_pipeline()` orphan-killing) are replaced with **Rust-native crates**:
//!
//! * `async-compression` — the hot `zstd` stage is fully async-streamed (tokio
//!   `ZstdEncoder`/`ZstdDecoder`), chosen per the issue's recommended Q1-(b).
//! * `tar` — creates/parses the tar stream **synchronously inside a
//!   `spawn_blocking` task** (it has no async API), bridged to/from the async
//!   `zstd` stage by a bounded `mpsc` channel.
//! * `walkdir` — enumerates `base` with `find . -mindepth 1` semantics (every
//!   descendant, no leading `.` entry) so restoring into a root-owned emptyDir
//!   mountpoint never makes tar try to chown/chmod the mountpoint itself.
//!
//! The whole archive is **never buffered in memory**: bytes flow through fixed
//! bounded channels (`CHANNEL_DEPTH`) and a fixed duplex buffer, so backpressure
//! from a slow HTTP client throttles the tar/zstd producer exactly like a shell
//! pipe did — and a dropped body reaps the producer (its channel writes error).
//!
//! ## Size safety (D9 fail-on-exceed)
//! * `/snapshot` pre-checks the apparent workspace size against
//!   `RuntimeConfig::max_workspace_bytes` and returns **413 before streaming**;
//! * `/restore` counts the COMPRESSED incoming bytes and aborts with **413** the
//!   instant the running total exceeds the cap (it never buffers the whole body).
//!
//! ## Error propagation (#82)
//! A mid-stream `zstd`/`tar` failure on `/restore` propagates as a **500**, never
//! a partial 200 — the response is only built once both the decode and extract
//! tasks have returned `Ok`. On `/snapshot` the 200/streaming response is already
//! committed before streaming begins (HTTP/1.1 chunked encoding cannot retroactively
//! become a 5xx), so a producer failure surfaces as a loudly-logged truncated stream.
//!
//! ## Path-traversal security (issue Q5)
//! Every restore entry is confined to `base` by an explicit guard
//! (`confine_entry`) BEFORE unpacking: entries resolving outside `base` (`..`,
//! absolute, or a symlink/hardlink whose target escapes) abort the restore with a
//! **500** and write nothing outside `base`. This is layered on top of the `tar`
//! crate's own traversal guard — defense-in-depth.

#![forbid(unsafe_code)]

use std::convert::Infallible;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::task::{ready, Context, Poll};

use async_compression::tokio::bufread::{ZstdDecoder, ZstdEncoder};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use http_body_util::channel::Channel;
use http_body_util::BodyExt;
use serde::Serialize;
use tar::{Archive, Builder};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadBuf};
use tokio::sync::mpsc;
use walkdir::WalkDir;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::request_base;
use crate::state::AppState;

/// Streaming read chunk (1 MiB).
const CHUNK: usize = 1 << 20;
/// Bounded frames in flight between the producer task and the response body.
const BODY_CHANNEL_DEPTH: usize = 8;
/// Bounded `Bytes` frames in flight across the sync↔async `tar` bridges, and the
/// backpressure window for the restore duplex. Mirrors the shell-pipe buffer: a
/// slow HTTP consumer throttles the `tar`/`zstd` producer instead of buffering.
const CHANNEL_DEPTH: usize = 8;
/// Byte capacity of the restore duplex that carries the COMPRESSED request body
/// from the handler into the async `ZstdDecoder`.
const DUPLEX_BUF: usize = 64 * 1024;
/// `zstd -3` level (issue Q6) — matches the former `zstd -3 -q` exactly.
const ZSTD_LEVEL: i32 = 3;

/// Response body for `PUT /restore`: whether the workspace was restored and the
/// compressed bytes ingested. `restored: false` with `skipped:
/// Some("workspace-non-empty")` is the #142 hot-tier hit: the caller (broker)
/// asked for a cold restore, but the mounted workspace already has data (PVC
/// park-resume), and unpacking the cold object over it could regress newer
/// state — so the runtime declines and keeps serving the hot tier.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RestoreResponse {
    restored: bool,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<&'static str>,
}

/// Whether the workspace root holds any USER entry (#142 restore-if-empty gate).
///
/// `.open-websandbox` is the runtime's own reserved namespace and does NOT
/// count: the SIGTERM scrollback flush (#129) recreates
/// `.open-websandbox/scrollback` under the workspace as the pod dies — i.e.
/// AFTER a reap-time purge — so a freshly created PVC pod can legitimately
/// carry that directory while having no user data to serve from the hot tier.
fn workspace_non_empty(base: &std::path::Path) -> bool {
    std::fs::read_dir(base)
        .is_ok_and(|mut it| it.any(|e| e.is_ok_and(|e| e.file_name() != ".open-websandbox")))
}

// ---------------------------------------------------------------------------
// GET /snapshot
// ---------------------------------------------------------------------------

/// `GET /snapshot` — stream the workspace as a zstd-compressed tar.
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

    // Pre-check: refuse BEFORE opening any stream (D9 fail-on-exceed).
    if workspace_size(&base) > state.config.max_workspace_bytes {
        return Err(ApiError::PayloadTooLarge(
            "workspace exceeds MAX_WORKSPACE_BYTES".to_string(),
        ));
    }

    // sync→async bridge: the `spawn_blocking` tar builder (sync `Write`) feeds the
    // async `ZstdEncoder` (async `Read`) through a bounded channel.
    let (tar_tx, tar_rx) = mpsc::channel::<Bytes>(CHANNEL_DEPTH);
    let base_for_tar = base.clone();
    let tar_task = tokio::task::spawn_blocking(move || build_archive(&base_for_tar, tar_tx));

    // `ZstdEncoder` is fully async-streamed (Q1-b); its source is the bridge reader.
    let reader = ChannelReader::new(tar_rx);
    let encoder = ZstdEncoder::with_quality(
        BufReader::new(reader),
        async_compression::Level::Precise(ZSTD_LEVEL),
    );

    // Stream the encoder's compressed output into the response body from a producer
    // task. The `Channel` is a native body (no manual `Stream` impl): `send_data`
    // resolves to `Err` once the response body is dropped (client gone), at which
    // point we stop reading — and dropping the bridge reader errors the producer's
    // channel writes, reaping it without orphans.
    let (mut sender, body_rx) = Channel::<Bytes, Infallible>::new(BODY_CHANNEL_DEPTH);
    tokio::spawn(async move {
        let mut enc = encoder;
        let mut buf = vec![0u8; CHUNK];
        loop {
            match enc.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    if sender.send_data(chunk).await.is_err() {
                        // Body receiver gone (client disconnect) — stop draining.
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "snapshot: zstd encode read error");
                    break;
                }
            }
        }
        // The encoder finalizes its zstd frame on the source-EOF `Ok(0)` above, so a
        // clean loop exit is a complete stream; a break on disconnect leaves a
        // truncated stream (client gone anyway). Surface the producer's result: a
        // non-zero tar build here means the client received a TRUNCATED archive (the
        // 200 status was already committed before streaming began — HTTP/1.1 chunked
        // encoding cannot retroactively become a 5xx, see module docs).
        match tar_task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(
                error = %e,
                "snapshot archive build failed after streaming began; \
                 client received a truncated workspace archive (200 already committed)"
            ),
            Err(e) => tracing::error!(error = %e, "snapshot tar task join failed"),
        }
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

/// Build a tar of every descendant of `base` (`find . -mindepth 1` semantics: all
/// entries, no leading `.` entry) and stream it through the sync→async bridge.
///
/// Runs on a `spawn_blocking` thread: the `tar` crate is synchronous, so it writes
/// into a [`ChannelWriter`] whose `blocking_send` applies natural backpressure when
/// the async `zstd` stage is not draining (bounded channel = shell-pipe buffer).
fn build_archive(base: &Path, tx: mpsc::Sender<Bytes>) -> io::Result<()> {
    let buf = std::io::BufWriter::with_capacity(CHUNK, ChannelWriter::new(tx));
    let mut builder = Builder::new(buf);
    // GNU tar (and the former `tar --null --no-recursion` pipeline) does NOT
    // dereference symlinks: a symlink is archived as a symlink, never followed.
    // The `tar` crate defaults `follow_symlinks(true)`, so override it — this
    // also matches `workspace_size`, which never follows symlink targets.
    builder.follow_symlinks(false);

    // Deterministic (sorted) traversal for reproducible archive bytes (Q6): GNU
    // `find` uses readdir order, which is unspecified; sorting is stable across
    // runs and is irrelevant to the logical-content interop contract (Q3).
    let mut entries: Vec<PathBuf> = WalkDir::new(base)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| match e {
            Ok(e) => Some(e.path().to_path_buf()),
            // A vanished/unreadable entry during the walk is skipped best-effort
            // (`except OSError: continue`-style) — a snapshot is not
            // failed because a single file raced out from under us.
            Err(e) => {
                tracing::debug!(error = %e, "snapshot: skipping unreadable entry");
                None
            }
        })
        .collect();
    entries.sort();

    for path in &entries {
        // Archive name is the path relative to `base` (no leading `./`), exactly
        // like `tar -T -` archived the `find ./...` paths. `append_path_with_name`
        // stats with `symlink_metadata`, so symlinks are archived as symlinks
        // (never followed) and directories emit a directory entry.
        let rel = path.strip_prefix(base).unwrap_or(path);
        builder.append_path_with_name(path, rel)?;
    }

    // `into_inner` writes the two 512-byte EOF blocks and returns the `BufWriter`;
    // flushing it pushes the tail into the channel, and dropping it drops the only
    // `Sender`, closing the bridge so the async reader sees EOF.
    let mut buf = builder.into_inner()?;
    buf.flush()?;
    drop(buf);
    Ok(())
}

// ---------------------------------------------------------------------------
// PUT /restore
// ---------------------------------------------------------------------------

/// `PUT /restore` — unpack a zstd-compressed tar back into the workspace.
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

    // Restore-if-empty (#142): with a PVC hot tier the workspace survives pod
    // delete, so a restore request can arrive while hot data is already
    // mounted (park resume, or a purge that failed after a previous offload).
    // Only this side can see the mount — if anything is present, decline with
    // `restored: false` instead of unpacking a possibly-stale cold object over
    // newer hot data. emptyDir callers are unaffected: a fresh pod's workspace
    // is always empty, so the restore proceeds as before.
    if workspace_non_empty(&base) {
        // Drain the request body so the connection can be reused cleanly (the
        // broker always sends the full object; bounded by its size cap).
        tokio::spawn(async move {
            let _ = http_body_util::BodyExt::collect(body).await;
        });
        tracing::info!("restore skipped: workspace non-empty (hot-tier hit)");
        return Ok(Json(RestoreResponse {
            restored: false,
            bytes: 0,
            skipped: Some("workspace-non-empty"),
        }));
    }

    // async→sync bridge: the async `ZstdDecoder` (decompressed tar bytes) feeds the
    // `spawn_blocking` `tar::Archive` extractor (sync `Read`) through a bounded
    // channel.
    let (dec_tx, dec_rx) = mpsc::channel::<Bytes>(CHANNEL_DEPTH);
    let base_for_tar = base.clone();
    let extract_task = tokio::task::spawn_blocking(move || {
        extract_archive(&base_for_tar, SyncChannelReader::new(dec_rx))
    });

    // The COMPRESSED request body flows handler → duplex → `ZstdDecoder`. Counting
    // compressed bytes for the 413 cap happens in the handler loop below (before any
    // byte is handed to the decoder), so a too-large body is rejected pre-decode.
    let (dr, mut dw) = tokio::io::duplex(DUPLEX_BUF);
    let decode_task = tokio::spawn(async move {
        let mut decoder = ZstdDecoder::new(BufReader::new(dr));
        let mut buf = vec![0u8; CHUNK];
        loop {
            match decoder.read(&mut buf).await {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    // Extractor gone (bad tar) — stop decoding; the extractor error
                    // is reported via its joined result below.
                    if dec_tx
                        .send(Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                // Corrupt/truncated zstd (#82): surface as an error so the handler
                // returns a 500, never a silent 200.
                Err(e) => return Err(e),
            }
        }
    });

    // Stream the request body into the duplex, counting COMPRESSED bytes and
    // aborting the instant the running total exceeds the cap (the stream breaks before
    // writing the chunk that crosses the limit).
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
        // trailers frame — ignore
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > cap {
            exceeded = true;
            break;
        }
        if dw.write_all(&chunk).await.is_err() {
            // Downstream (decoder/extractor) exited early on bad input; reported
            // via the joined results below.
            break;
        }
    }
    // Dropping the write half sends EOF so the decoder (then extractor) terminate.
    drop(dw);

    if exceeded {
        // The spawn_blocking extractor is left to finish what it has and drop on its
        // own (its bridge sender is dropped when the decoder task ends).
        return Err(ApiError::PayloadTooLarge(format!(
            "restore stream exceeds MAX_WORKSPACE_BYTES ({cap})"
        )));
    }

    if let Some(msg) = body_err {
        return Err(ApiError::Internal(format!(
            "restore stream read failed: {msg}"
        )));
    }

    // Decode first: its completion drops `dec_tx`, giving the extractor EOF so it
    // can finish. A corrupt/truncated zstd stream surfaces here (#82) → 500.
    let decode_res = match decode_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, base = %base.display(), "restore zstd decode failed");
            Err(ApiError::Internal(format!(
                "restore pipeline failed (zstd decode): {e}"
            )))
        }
        Err(e) => Err(ApiError::Internal(format!(
            "restore decode task join failed: {e}"
        ))),
    };
    decode_res?;

    // Corrupt tar OR a path-traversal rejection (Q5) surfaces here → 500.
    let extract_res = match extract_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, base = %base.display(), "restore tar extract failed");
            Err(ApiError::Internal(format!(
                "restore pipeline failed (tar extract): {e}"
            )))
        }
        Err(e) => Err(ApiError::Internal(format!(
            "restore extract task join failed: {e}"
        ))),
    };
    extract_res?;

    tracing::info!(base = %base.display(), received, "restore into workspace");
    Ok(Json(RestoreResponse {
        restored: true,
        bytes: received,
        skipped: None,
    }))
}

/// Extract a decompressed tar stream into `base` with an explicit path-confinement
/// guard ([`confine_entry`]) layered on the `tar` crate's own guard (defense in
/// depth, issue Q5). Runs on a `spawn_blocking` thread.
fn extract_archive<R: Read>(base: &Path, reader: R) -> io::Result<()> {
    let mut ar = Archive::new(reader);
    // GNU tar preserves mode bits by default; match it on unix (no-op elsewhere).
    ar.set_preserve_permissions(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        let header = entry.header();
        let is_link = header.entry_type().is_symlink() || header.entry_type().is_hard_link();
        let link: Option<PathBuf> = if is_link {
            header.link_name()?.map(std::borrow::Cow::into_owned)
        } else {
            None
        };
        if !confine_entry(&name, link.as_deref(), base) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "restore archive entry escapes workspace: {}",
                    name.display()
                ),
            ));
        }
        // `unpack_in` applies the tar crate's own traversal guard too — two
        // independent checks (ours first, then theirs) before any byte is written.
        entry.unpack_in(base)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// path confinement (issue Q5)
// ---------------------------------------------------------------------------

/// Lexical path confinement: does `rel` (relative to `base`) resolve to a path
/// at or under `base`? Rejects absolute entries (`/`, `C:\`) and any `..` that
/// would climb above `base`. Pure lexical analysis — no symlink resolution, so it
/// is immune to TOCTOU; a symlink created by a prior entry is checked separately
/// by [`link_within`] on the link target itself.
fn lexically_within(base: &Path, rel: &Path) -> bool {
    let mut cur = base.to_path_buf();
    for c in rel.components() {
        match c {
            Component::Normal(n) => cur.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            // Absolute entry or a Windows prefix — never allowed into a relative base.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    cur.starts_with(base)
}

/// Confinement for a symlink/hardlink target: the target resolves relative to the
/// link's own parent directory (as a real symlink does), and the resolved path must
/// stay under `base`. Absolute targets and `..` escapes are rejected.
fn link_within(base: &Path, link_name: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut cur = base.to_path_buf();
    if let Some(parent) = link_name.parent() {
        cur.push(parent);
    }
    for c in target.components() {
        match c {
            Component::Normal(n) => cur.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                cur.pop();
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    cur.starts_with(base)
}

/// Validate a restore entry: its path must stay under `base`, and (for symlinks
/// and hardlinks) its link target must too.
fn confine_entry(name: &Path, link: Option<&Path>, base: &Path) -> bool {
    if !lexically_within(base, name) {
        return false;
    }
    if let Some(target) = link {
        if !link_within(base, name, target) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// sync↔async channel bridges
// ---------------------------------------------------------------------------

/// Sync `Write` → bounded `mpsc::Sender<Bytes>`, used from a `spawn_blocking`
/// thread. `blocking_send` applies backpressure when the async consumer (the
/// `zstd` stage) is not draining; when the consumer is dropped (client gone /
/// body aborted) `blocking_send` errors and the producer stops — no orphans.
struct ChannelWriter {
    tx: mpsc::Sender<Bytes>,
}

impl ChannelWriter {
    fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self { tx }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.tx.blocking_send(Bytes::copy_from_slice(buf)) {
            Ok(()) => Ok(buf.len()),
            // Consumer gone: signal a broken pipe so the tar builder aborts.
            Err(_) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "snapshot bridge closed",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Async `Read` ← bounded `mpsc::Receiver<Bytes>`, the async counterpart to
/// [`ChannelWriter`]. Feeds the async `ZstdEncoder`; EOF when the producer drops
/// its only `Sender`.
struct ChannelReader {
    rx: mpsc::Receiver<Bytes>,
    pending: Bytes,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            rx,
            pending: Bytes::new(),
        }
    }
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pending.is_empty() {
            // `poll_recv` is `Cancel-safe`; returning Pending here is fine.
            match ready!(Pin::new(&mut self.rx).poll_recv(cx)) {
                Some(b) => self.pending = b,
                None => return Poll::Ready(Ok(())), // producer closed → EOF
            }
        }
        let n = std::cmp::min(self.pending.len(), buf.remaining());
        buf.put_slice(&self.pending[..n]);
        self.pending = self.pending.slice(n..);
        Poll::Ready(Ok(()))
    }
}

/// Sync `Read` ← bounded `mpsc::Receiver<Bytes>`, the sync counterpart to the
/// async decoder. `blocking_recv` blocks the `spawn_blocking` thread (never the
/// runtime) until the next chunk; EOF when the decoder drops its `Sender`.
struct SyncChannelReader {
    rx: mpsc::Receiver<Bytes>,
    pending: Bytes,
}

impl SyncChannelReader {
    fn new(rx: mpsc::Receiver<Bytes>) -> Self {
        Self {
            rx,
            pending: Bytes::new(),
        }
    }
}

impl Read for SyncChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.blocking_recv() {
                Some(b) => self.pending = b,
                None => return Ok(0), // decoder closed → EOF
            }
        }
        let n = std::cmp::min(self.pending.len(), buf.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending = self.pending.slice(n..);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Apparent bytes of everything under `base` (snapshot pre-check).
///
/// Walks with `symlink_metadata` so a symlink counts its OWN (link) size, never
/// its target's, and never recurses INTO a symlinked directory (matches the
/// `os.walk(followlinks=False)` shape). Files that vanish mid-walk are skipped,
/// (`except OSError: continue`-style).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SessionKeyStore;
    use crate::config::RuntimeConfig;
    use crate::state::AppState;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::io::Cursor;
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

    /// Compress `data` with the same async `zstd` stage the runtime uses, for
    /// synthesizing restore inputs in tests.
    async fn zstd_compress(data: Vec<u8>) -> Vec<u8> {
        let mut enc = ZstdEncoder::with_quality(
            BufReader::new(Cursor::new(data)),
            async_compression::Level::Precise(ZSTD_LEVEL),
        );
        let mut out = Vec::new();
        enc.read_to_end(&mut out).await.unwrap();
        out
    }

    /// Build one raw tar entry (512-byte header + data + zero padding) with an
    /// ARBITRARY name/linkname/typeflag, bypassing the `tar` crate's own `..`
    /// guard (which refuses to construct such entries at all). Used only to craft
    /// malicious archives for the path-traversal test below.
    fn raw_tar_entry(name: &[u8], typeflag: u8, linkname: &[u8], data: &[u8]) -> Vec<u8> {
        assert!(name.len() <= 100 && linkname.len() <= 100);
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name);
        header[100..108].copy_from_slice(b"0000644\0"); // mode
        header[108..116].copy_from_slice(b"0000000\0"); // uid
        header[116..124].copy_from_slice(b"0000000\0"); // gid
        let size_oct = format!("{:011o}", data.len());
        header[124..135].copy_from_slice(size_oct.as_bytes());
        header[135] = 0; // size NUL terminator
        header[136..147].copy_from_slice(b"00000000000"); // mtime=0
        header[147] = 0;
        for b in &mut header[148..156] {
            *b = b' ';
        }
        header[156] = typeflag;
        header[157..157 + linkname.len()].copy_from_slice(linkname);
        header[257..263].copy_from_slice(b"ustar\0"); // magic
        header[263..265].copy_from_slice(b"00"); // version
        let cksum: u64 = header.iter().map(|&b| u64::from(b)).sum();
        header[148..156].copy_from_slice(format!("{cksum:06o}\0 ").as_bytes());
        let mut out = header.to_vec();
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.resize(out.len() + pad, 0);
        out
    }

    /// Craft a malicious `.tar.zst` from raw header bytes (the `tar` crate
    /// itself refuses to build these): a `..` traversal entry, an absolute entry,
    /// and a symlink whose target escapes the workspace.
    async fn malicious_tar_zst(escape_target: &str) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        tar_buf.extend_from_slice(&raw_tar_entry(b"../OUTSIDE_MARKER", b'0', b"", b"pwned"));
        tar_buf.extend_from_slice(&raw_tar_entry(b"/absolute/evil", b'0', b"", b"abs"));
        tar_buf.extend_from_slice(&raw_tar_entry(b"lnk", b'2', escape_target.as_bytes(), b""));
        tar_buf.resize(tar_buf.len() + 1024, 0); // two 512-byte EOF blocks
        zstd_compress(tar_buf).await
    }

    // --- confinement unit tests (Q5) ---------------------------------------

    #[test]
    fn lexically_within_accepts_nested_rejects_escape() {
        let base = Path::new("/ws");
        assert!(lexically_within(base, Path::new("a.txt")));
        assert!(lexically_within(base, Path::new("sub/deep/x.bin")));
        assert!(lexically_within(base, Path::new("a/../b.txt")));
        // `..` escapes base.
        assert!(!lexically_within(base, Path::new("../escape")));
        assert!(!lexically_within(base, Path::new("sub/../../escape")));
        // Absolute entries rejected outright.
        assert!(!lexically_within(base, Path::new("/etc/evil")));
        assert!(!lexically_within(base, Path::new("/ws/../etc")));
    }

    #[test]
    fn link_within_rejects_symlink_escape() {
        let base = Path::new("/ws");
        // In-base symlink target is fine.
        assert!(link_within(
            base,
            Path::new("link"),
            Path::new("target.txt")
        ));
        assert!(link_within(
            base,
            Path::new("sub/l"),
            Path::new("../sibling")
        ));
        // Absolute target and `..` escapes rejected.
        assert!(!link_within(
            base,
            Path::new("link"),
            Path::new("/etc/passwd")
        ));
        assert!(!link_within(
            base,
            Path::new("link"),
            Path::new("../../etc/passwd")
        ));
    }

    // --- #82 regression: corrupt zstd -> 500 --------------------------------

    /// #82 error path: a mid-stream zstd failure on `/restore` must propagate as a
    /// 500, NOT a silent 200 with a truncated body. Bytes without the zstd magic
    /// make the decoder error, which the restore handler must turn into
    /// `ApiError::Internal`.
    #[tokio::test]
    async fn restore_invalid_zstd_returns_error_not_ok() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        // Definitely-not-zstd body (no 0x28B52FFD magic).
        let garbage = b"this is definitely not a zstd stream".to_vec();
        let res = restore(Authed, State(state), Body::from(garbage)).await;
        assert!(
            matches!(res, Err(ApiError::Internal(_))),
            "expected Err(Internal) (500) on invalid zstd, got {res:?}"
        );
    }

    // --- happy-path anchor + #82 corrupt-restore regression ----------------

    #[tokio::test]
    async fn snapshot_restore_roundtrip_and_error_path() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"alpha")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"beta beta")
            .await
            .unwrap();
        let state = make_state(&dir);

        // snapshot -> collect the streamed body.
        let resp = snapshot(Authed, State(state.clone()))
            .await
            .expect("snapshot ok");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        assert!(body.len() >= 4, "snapshot body too small");
        // zstd magic: 0x28 0xB5 0x2F 0xFD.
        assert_eq!(
            &body[..4],
            &[0x28, 0xB5, 0x2F, 0xFD],
            "snapshot body is not a zstd stream"
        );

        // Restore the valid archive — into the EMPTY workspace (fresh-pod
        // semantics: the broker only restores when nothing is mounted, #142).
        tokio::fs::remove_file(dir.path().join("a.txt"))
            .await
            .unwrap();
        tokio::fs::remove_file(dir.path().join("b.txt"))
            .await
            .unwrap();
        let ok = restore(Authed, State(state.clone()), Body::from(body.to_vec()))
            .await
            .expect("restore ok");
        assert!(ok.restored, "restore reported not restored");
        assert!(dir.path().join("a.txt").exists(), "restored file missing");
        assert!(dir.path().join("b.txt").exists(), "restored file missing");

        // Corrupt restore — must be a 500, not a silent success (#82). The
        // restore-if-empty gate needs an empty workspace to even attempt the
        // decode, so clear the just-restored files first.
        tokio::fs::remove_file(dir.path().join("a.txt"))
            .await
            .unwrap();
        tokio::fs::remove_file(dir.path().join("b.txt"))
            .await
            .unwrap();
        let bad = restore(
            Authed,
            State(state),
            Body::from(b"not zstd garbage".to_vec()),
        )
        .await;
        assert!(
            matches!(bad, Err(ApiError::Internal(_))),
            "expected Err(Internal) on corrupt restore, got {bad:?}"
        );
    }

    #[tokio::test]
    async fn restore_proceeds_when_only_reserved_dir_present() {
        // #129 recreates `.open-websandbox/scrollback` as the pod DIES (after a
        // reap-time purge) — a fresh PVC pod can carry exactly that dir and no
        // user data. It must NOT block the cold restore (#142).
        let src = TempDir::new().unwrap();
        tokio::fs::write(src.path().join("cold.txt"), b"cold user data")
            .await
            .unwrap();
        let cold = {
            let state = make_state(&src);
            let resp = snapshot(Authed, State(state)).await.expect("snapshot ok");
            resp.into_body().collect().await.unwrap().to_bytes()
        };

        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".open-websandbox/scrollback")).unwrap();
        let state = make_state(&dir);
        let res = restore(Authed, State(state), Body::from(cold.to_vec())).await;
        let ok = res.expect("restore over reserved-dir-only workspace");
        assert!(ok.restored, "reserved dir must not block restore");
        assert!(dir.path().join("cold.txt").exists(), "user data restored");
        assert!(dir.path().join(".open-websandbox/scrollback").exists());
    }

    #[tokio::test]
    async fn restore_skips_when_workspace_non_empty() {
        // #142 hot-tier hit: the cold object exists, but the PVC workspace
        // still has data. The runtime must decline (restored: false) and
        // leave the hot data untouched — never unpack stale-cold over hot.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("hot.txt"), b"hot data v2").unwrap();
        let state = make_state(&dir);
        let cold = zstd_compress(b"stale cold data v1".to_vec()).await;
        let res = restore(Authed, State(state), Body::from(cold)).await;
        let ok = res.expect("skip is a 200, not an error");
        assert!(!ok.restored, "must not restore over hot data");
        assert_eq!(ok.skipped, Some("workspace-non-empty"));
        assert_eq!(
            std::fs::read(dir.path().join("hot.txt")).unwrap(),
            b"hot data v2",
            "hot data must be untouched"
        );
    }

    // --- rich round-trips: empty, nested, empty file, binary, symlink, unicode ---

    #[tokio::test]
    async fn roundtrip_empty_workspace() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        let resp = snapshot(Authed, State(state.clone()))
            .await
            .expect("snapshot ok");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        let ok = restore(Authed, State(state), Body::from(body.to_vec()))
            .await
            .expect("restore ok");
        assert!(ok.restored);
        // Empty workspace stays empty (no entries written).
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn roundtrip_nested_empty_file_binary_symlink_unicode() {
        let dir = TempDir::new().unwrap();
        // Deeply nested dirs.
        let deep = dir.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        // Empty file.
        std::fs::write(deep.join("empty"), b"").unwrap();
        // Binary blob (>1 chunk).
        let blob: Vec<u8> = (0u8..=255).cycle().take(CHUNK * 2 + 7).collect();
        std::fs::write(dir.path().join("blob.bin"), &blob).unwrap();
        // Symlink (in-workspace, relative).
        std::os::unix::fs::symlink("blob.bin", dir.path().join("link.bin")).unwrap();
        // Unicode + special-char filename.
        let uni = "café-müller_数据_<tag> & more.txt";
        std::fs::write(dir.path().join(uni), "héllo wörld 🌍".as_bytes()).unwrap();

        let state = make_state(&dir);
        let resp = snapshot(Authed, State(state.clone()))
            .await
            .expect("snapshot ok");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();

        // Restore into a FRESH workspace.
        let dst = TempDir::new().unwrap();
        let dst_state = make_state(&dst);
        let ok = restore(Authed, State(dst_state), Body::from(body.to_vec()))
            .await
            .expect("restore ok");
        assert!(ok.restored);

        assert_eq!(
            std::fs::read(dst.path().join("a/b/c/d/empty")).unwrap(),
            b""
        );
        assert_eq!(std::fs::read(dst.path().join("blob.bin")).unwrap(), blob);
        assert_eq!(
            std::fs::read_link(dst.path().join("link.bin"))
                .unwrap()
                .to_string_lossy(),
            "blob.bin"
        );
        assert!(dst.path().join("link.bin").is_symlink());
        assert_eq!(
            std::fs::read_to_string(dst.path().join(uni)).unwrap(),
            "héllo wörld 🌍"
        );
    }

    // --- error cases: truncated zstd, corrupt tar -------------------------

    #[tokio::test]
    async fn restore_truncated_zstream_returns_500() {
        // Snapshot a real workspace, then truncate the valid stream mid-zstd-frame.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"some bytes to compress").unwrap();
        let state = make_state(&dir);
        let resp = snapshot(Authed, State(state.clone()))
            .await
            .expect("snapshot ok");
        let full = resp.into_body().collect().await.unwrap().to_bytes();
        std::fs::remove_file(dir.path().join("f.txt")).unwrap(); // fresh pod (#142)
                                                                 // Cutting the stream in half lands inside a zstd frame: the decoder hits an
                                                                 // unexpected end-of-stream, which (#82) must surface as a 500, not a 200.
        let truncated = &full[..full.len() / 2];
        let res = restore(Authed, State(state), Body::from(truncated.to_vec())).await;
        assert!(
            matches!(res, Err(ApiError::Internal(_))),
            "truncated zstd must be a 500, got {res:?}"
        );
    }

    #[tokio::test]
    async fn restore_corrupt_tar_returns_500() {
        let dir = TempDir::new().unwrap();
        let state = make_state(&dir);
        // Valid zstd stream wrapping garbage that is NOT a valid tar header.
        let bad = zstd_compress(b"not a tar archive at all, just junk bytes".to_vec()).await;
        let res = restore(Authed, State(state), Body::from(bad)).await;
        assert!(
            matches!(res, Err(ApiError::Internal(_))),
            "corrupt tar must be a 500, got {res:?}"
        );
    }

    // --- restore 413 compressed-byte cap -----------------------------------

    #[tokio::test]
    async fn restore_oversize_returns_413() {
        let dir = TempDir::new().unwrap();
        let mut config = RuntimeConfig {
            workdir: dir.path().to_path_buf(),
            ..Default::default()
        };
        config.max_workspace_bytes = 64;
        let state = AppState::new(config, SessionKeyStore::new(dir.path().join("api-key")));
        let res = restore(Authed, State(state), Body::from(vec![b'x'; 4096])).await;
        assert!(
            matches!(res, Err(ApiError::PayloadTooLarge(_))),
            "oversize restore must be a 413, got {res:?}"
        );
    }

    // --- path-traversal security (issue Q5) --------------------------------

    #[tokio::test]
    async fn restore_rejects_path_traversal_nothing_outside_base() {
        // A sentinel sibling of `base` to prove restore never escapes.
        let jail = TempDir::new().unwrap();
        let ws = jail.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        // Marker that must NOT be created by restore.
        let escape_target = jail.path().join("OUTSIDE_MARKER");
        assert!(!escape_target.exists());

        let config = RuntimeConfig {
            workdir: ws.clone(),
            ..Default::default()
        };
        let state = AppState::new(config, SessionKeyStore::new(jail.path().join("api-key")));

        let target_str = escape_target.to_string_lossy().to_string();
        let evil = malicious_tar_zst(&target_str).await;

        let res = restore(Authed, State(state), Body::from(evil)).await;
        assert!(
            matches!(res, Err(ApiError::Internal(_))),
            "traversal archive must be rejected (500), got {res:?}"
        );
        // Defense-in-depth guarantee: nothing outside `base` was written, and the
        // symlink-escape target was never followed/created.
        assert!(
            !escape_target.exists(),
            "escape marker was created outside base!"
        );
        assert!(
            std::fs::read_dir(jail.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .all(|e| e.file_name() == "workspace" || e.file_name() == "api-key"),
            "jail directory gained unexpected entries outside base"
        );
    }
}
