//! Interactive terminal surface — `portable-pty` + axum WebSocket relay.
//!
//! Rust PTY surface (the
//! `/api/terminals` POST/GET/DELETE + the `/api/terminals/{id}` WebSocket relay).
//!
//! # Wire contract (D5 / D11 — strict 1:1)
//!
//! * `POST /api/terminals` (Authed) → `200 {"id","created_at","pid"}`; spawns the
//!   configured `$SHELL` on a 24×80 PTY in `request_base(workdir, X-Workspace-Subdir)`
//!   (terminals DO honour the subdir header, unlike snapshot/restore — D11). Reaps
//!   dead sessions, recreates an existing id, and answers **429** once
//!   `MAX_TERMINAL_SESSIONS` live sessions are reached; **503** on PTY spawn failure.
//! * `GET /api/terminals` (Authed) → `200 [{"id","created_at","pid"}, …]`, reaping
//!   any dead entries it observes.
//! * `GET /api/terminals/{id}` (Authed) → `200 {"id","created_at","pid"}` / **404**.
//! * `DELETE /api/terminals/{id}` (Authed) → `200 {"status":"deleted"}` (idempotent).
//! * `GET /api/terminals/{id}` WebSocket (Authed at upgrade — 401 before the socket
//!   is accepted): **1:1 binary/text frames** relayed byte-for-byte.
//!   Inbound **binary** frames are raw stdin to the PTY; inbound **text** frames are
//!   JSON control messages (`{"type":"resize","rows":N,"cols":M}`, tolerated
//!   `{"type":"auth",…}`); PTY output is relayed back as **binary** frames. An
//!   unknown/dead session is closed with code **4004**.
//!
//! # Clean shutdown (no zombie)
//!
//! Each `Session` owns the `portable-pty` master + child. `portable-pty` spawns the
//! shell with `setsid()`, so the shell is its own session/process-group leader
//! (pgid == pid) — equivalent to `start_new_session=True`. On teardown
//! (WS close, `DELETE`, dead-session reap, or process exit) we `killpg(SIGKILL)` the
//! whole group (`os.killpg(os.getpgid(pid), SIGKILL)`) and
//! `child.wait()` to reap, so no zombie shell survives. The per-connection reader and
//! stdin-writer threads own independent `dup`'d master fds and self-terminate once
//! the child dies (the master read returns EOF/EIO) or the relay ends.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio::task::spawn_blocking;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::request_base;
use crate::state::AppState;

/// WebSocket close code for an unknown or already-ended session.
const CLOSE_UNKNOWN: u16 = 4004;
/// Default window size every PTY is created with (24×80).
const PTY_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};
/// Heartbeat interval for the relay: detects a dead shell whose PTY slave is still
/// held open by a backgrounded grandchild (no EOF/EIO reaches the master read).
const HEARTBEAT: Duration = Duration::from_secs(1);
/// Bounded channel capacity — backpressure so a slow WS client throttles the shell
/// (PTY→relay) and a blocked PTY input throttles the client (relay→PTY).
const OUT_CHAN: usize = 256;
const IN_CHAN: usize = 64;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-process terminal session registry, keyed by opaque session id. Held in
/// [`AppState`] behind an `Arc`; one entry per live PTY.
#[derive(Default)]
pub struct TerminalRegistry {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// One live PTY session. Shared between the registry and the (single) active WS
/// relay via `Arc<Mutex<Session>>`; cheap brief locks guard every operation.
struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    created_at: String,
    pid: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Safety net only: every normal teardown path (`teardown_one`) SIGKILLs the
        // group and `wait()`s to reap. If a `Session` is ever dropped without that,
        // best-effort `killpg` + a non-blocking `try_wait` keeps a shell from
        // outliving the process; the explicit reap in `teardown_one` is authoritative.
        let _ = killpg(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
        let _ = self.child.try_wait();
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TermInfo {
    id: String,
    created_at: String,
    pid: u32,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateResponse {
    id: String,
    created_at: String,
    pid: u32,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeleteResponse {
    status: &'static str,
}

/// Whether the session's child is still running.
async fn is_alive(session: &Arc<Mutex<Session>>) -> bool {
    session
        .lock()
        .await
        .child
        .try_wait()
        .ok()
        .flatten()
        .is_none()
}

/// SIGKILL the child's whole process group, then `wait()` to reap it (no zombie).
/// `portable-pty` spawns the shell via `setsid()`, so the group id equals the pid —
/// (`os.killpg(os.getpgid(pid), SIGKILL)`). The blocking reap
/// runs on a blocking thread so it never stalls the async runtime.
async fn teardown_one(session: Arc<Mutex<Session>>) {
    let pid = { session.lock().await.pid };
    // Instant: just delivers the signal. ESRCH (already gone) is ignored.
    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    let handle = session.clone();
    let _ = spawn_blocking(move || {
        let mut guard = handle.blocking_lock();
        // Best-effort reap. After SIGKILL the child dies promptly, so this returns
        // quickly; calling wait() twice (e.g. when Drop also runs) yields the cached
        // status, which is harmless.
        let _ = guard.child.wait();
    })
    .await;
}

// ---------------------------------------------------------------------------
// POST /api/terminals
// ---------------------------------------------------------------------------

/// `POST /api/terminals` — spawn a new PTY shell session.
#[utoipa::path(
    post,
    path = "/api/terminals",
    tag = "terminals",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "PTY session spawned (or recreated) — idempotent, returns 200 on create-or-recreate", body = CreateResponse),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse),
        (status = 429, description = "MAX_TERMINAL_SESSIONS reached", body = shared::ErrorResponse),
        (status = 503, description = "PTY spawn failure", body = shared::ErrorResponse)
    )
)]
pub async fn create_terminal(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CreateResponse>, ApiError> {
    let subdir = header_str(&headers, "x-workspace-subdir");
    let session_id = header_str(&headers, "x-session-id").map(str::to_owned);
    let base = request_base(&state.config.workdir, subdir)?;

    // Sessions torn down while holding the lock (dead reaping, recreate-existing)
    // are collected and reaped AFTER releasing it, so no other handler blocks on a
    // `wait()`.
    let mut to_teardown: Vec<Arc<Mutex<Session>>> = Vec::new();
    let cap = state.config.max_terminal_sessions;

    let resp = {
        let mut map = state.terminals.sessions.lock().await;

        // Reap dead sessions first (create-time sweep).
        let mut dead = Vec::new();
        for (k, s) in map.iter() {
            if !is_alive(s).await {
                dead.push(k.clone());
            }
        }
        for id in dead {
            if let Some(s) = map.remove(&id) {
                to_teardown.push(s);
            }
        }

        // Cap is checked AFTER reaping but BEFORE recreating an existing id
        // (`len(_terminals) >= MAX` ordering).
        if (map.len() as u32) >= cap {
            drop(map);
            for s in to_teardown {
                teardown_one(s).await;
            }
            return Err(ApiError::TooManyRequests(format!(
                "max {cap} terminals reached"
            )));
        }

        let id = session_id.unwrap_or_else(random_id);
        if let Some(existing) = map.remove(&id) {
            to_teardown.push(existing);
        }

        let session = match spawn_pty(&state.config.shell, &base) {
            Ok(s) => s,
            Err(e) => {
                drop(map);
                for s in to_teardown {
                    teardown_one(s).await;
                }
                return Err(ApiError::ServiceUnavailable(format!(
                    "pty spawn failed: {e}"
                )));
            }
        };
        let resp = CreateResponse {
            id: id.clone(),
            created_at: session.created_at.clone(),
            pid: session.pid,
        };
        map.insert(id, Arc::new(Mutex::new(session)));
        resp
    };

    for s in to_teardown {
        teardown_one(s).await;
    }
    Ok(Json(resp))
}

/// Build a PTY `Session`: `openpty` (24×80), spawn `$SHELL` in `cwd` with
/// `TERM=xterm-256color`, take the writer/reader handles lazily (per WS connection).
fn spawn_pty(shell: &str, cwd: &Path) -> Result<Session, String> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PTY_SIZE).map_err(|e| format!("{e}"))?;
    let mut cmd = CommandBuilder::new(shell);
    // `CommandBuilder::new` already inherits `os.environ`
    // (`{**os.environ, "TERM": ...}`); we only override TERM.
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd).map_err(|e| format!("{e}"))?;
    let pid = child.process_id().unwrap_or(0);
    let master = pair.master;
    // Close our slave handle so the child's fds are the only slave ends: when the
    // shell exits, the master read sees EOF/EIO (`os.close(slave_fd)`).
    drop(pair.slave);

    Ok(Session {
        master,
        child,
        created_at: now_iso_utc(),
        pid,
    })
}

// ---------------------------------------------------------------------------
// GET /api/terminals
// ---------------------------------------------------------------------------

/// `GET /api/terminals` — list live sessions, reaping any dead ones observed.
#[utoipa::path(
    get,
    path = "/api/terminals",
    tag = "terminals",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Live sessions", body = Vec<TermInfo>),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse)
    )
)]
pub async fn list_terminals(
    _auth: Authed,
    State(state): State<AppState>,
) -> Result<Json<Vec<TermInfo>>, ApiError> {
    let mut to_teardown = Vec::new();
    let out = {
        let mut map = state.terminals.sessions.lock().await;
        let mut out = Vec::new();
        let ids: Vec<String> = map.keys().cloned().collect();
        for id in ids {
            let Some(arc) = map.get(&id).cloned() else {
                continue;
            };
            if is_alive(&arc).await {
                let g = arc.lock().await;
                out.push(TermInfo {
                    id,
                    created_at: g.created_at.clone(),
                    pid: g.pid,
                });
            } else {
                map.remove(&id);
                to_teardown.push(arc);
            }
        }
        out
    };
    for s in to_teardown {
        teardown_one(s).await;
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// GET /api/terminals/{id}  +  WS upgrade  (same path, same method, branched)
// ---------------------------------------------------------------------------

/// `GET /api/terminals/{id}` — either the JSON info endpoint, or (when the request
/// carries the WebSocket upgrade headers) the PTY relay. `Authed` runs first, so a
/// missing/invalid Bearer is rejected with 401 BEFORE the socket is accepted.
#[utoipa::path(
    get,
    path = "/api/terminals/{id}",
    tag = "terminals",
    params(("id" = String, Path, description = "Terminal session id")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Session info for non-upgrade requests (the WebSocket upgrade relay is omitted from this document)", body = TermInfo),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse),
        (status = 404, description = "Unknown/ended session", body = shared::ErrorResponse)
    )
)]
pub async fn terminal_get_or_ws(
    _auth: Authed,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    match ws {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| relay(state, socket, id)),
        Err(_) => get_terminal(state, id).await.into_response(),
    }
}

async fn get_terminal(state: AppState, id: String) -> Result<Json<TermInfo>, ApiError> {
    let arc = { state.terminals.sessions.lock().await.get(&id).cloned() };
    match arc {
        Some(arc) if is_alive(&arc).await => {
            let g = arc.lock().await;
            Ok(Json(TermInfo {
                id,
                created_at: g.created_at.clone(),
                pid: g.pid,
            }))
        }
        Some(arc) => {
            // Dead: reap + remove, then 404.
            remove_if_eq(&state.terminals, &id, &arc).await;
            teardown_one(arc).await;
            Err(ApiError::NotFound("terminal not found".to_string()))
        }
        None => Err(ApiError::NotFound("terminal not found".to_string())),
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/terminals/{id}
// ---------------------------------------------------------------------------

/// `DELETE /api/terminals/{id}` — kill + reap the session. Idempotent: an unknown id
/// still answers `200 {"status":"deleted"}`.
#[utoipa::path(
    delete,
    path = "/api/terminals/{id}",
    tag = "terminals",
    params(("id" = String, Path, description = "Terminal session id")),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Session killed (idempotent: unknown id → 200)", body = DeleteResponse),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse)
    )
)]
pub async fn kill_terminal(
    _auth: Authed,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Json<DeleteResponse> {
    if let Some(arc) = state.terminals.sessions.lock().await.remove(&id) {
        teardown_one(arc).await;
    }
    Json(DeleteResponse { status: "deleted" })
}

// ---------------------------------------------------------------------------
// WebSocket relay
// ---------------------------------------------------------------------------

/// Bidirectional PTY↔WS relay for one connection. Runs until EITHER side ends
/// (client disconnect, PTY EOF, or a heartbeat-detected dead shell), then tears the
/// session down so the PTY is killed rather than leaking to the per-pod cap (→ 429).
async fn relay(state: AppState, mut socket: WebSocket, id: String) {
    let Some(session) = state.terminals.sessions.lock().await.get(&id).cloned() else {
        close_unknown(&mut socket).await;
        return;
    };

    if !is_alive(&session).await {
        remove_if_eq(&state.terminals, &id, &session).await;
        teardown_one(session).await;
        close_unknown(&mut socket).await;
        return;
    }

    // Take the (once-only) writer and a reader clone for this connection.
    let pair = {
        let g = session.lock().await;
        match (g.master.take_writer(), g.master.try_clone_reader()) {
            (Ok(w), Ok(r)) => Some((w, r)),
            _ => None,
        }
    };
    let (writer, reader) = match pair {
        Some(p) => p,
        None => {
            close_unknown(&mut socket).await;
            return;
        }
    };

    // Stdin pump: a dedicated OS thread owns the blocking master writer so a full PTY
    // input buffer never stalls the async runtime. A bounded channel applies
    // backpressure: if the shell stops reading, the relay stops draining the socket.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(IN_CHAN);
    let writer_thread = std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(bytes) = stdin_rx.blocking_recv() {
            // Best-effort; a closed PTY surfaces as an error we simply drop.
            let _ = writer.write_all(&bytes);
        }
        // `writer` drops → EOF to the slave (helps the shell notice a gone client).
    });

    // Output pump: another OS thread drains the blocking master reader and forwards
    // each chunk to the relay via a bounded channel.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(OUT_CHAN);
    let reader_thread = std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                // portable-pty maps the Linux EIO-on-slave-close to Ok(0), so EOF and
                // read errors both end the pump.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break; // relay gone — stop reading
                    }
                }
            }
        }
    });

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    // The first interval tick fires immediately; skip it so we don't tear down a
    // healthy session before its first output.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            // WS → PTY (stdin) / control frames.
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    // Channel-full applies backpressure: send().await parks the relay
                    // (and thus socket draining) until the writer catches up.
                    let _ = stdin_tx.send(bytes.to_vec()).await;
                }
                Some(Ok(Message::Text(text))) => {
                    apply_control(&text, &session).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                // Ping/Pong are auto-answered by axum; ignore everything else.
                _ => {}
            },
            // PTY → WS (stdout).
            chunk = out_rx.recv() => match chunk {
                Some(c) => {
                    if socket.send(Message::Binary(Bytes::from(c))).await.is_err() {
                        break;
                    }
                }
                None => break, // reader ended (shell exited → EOF/EIO)
            },
            // Heartbeat: catches a dead shell whose slave is still held open by a
            // backgrounded grandchild (no EOF reaches the master read).
            _ = heartbeat.tick() => {
                if !is_alive(&session).await {
                    break;
                }
            }
        }
    }

    // --- teardown -----------------------------------------------------------
    // Drop the channels so the writer pump flushes+exits; the reader pump exits once
    // the child is killed (master read → EOF/EIO).
    drop(stdin_tx);
    drop(out_rx);
    remove_if_eq(&state.terminals, &id, &session).await;
    teardown_one(session).await;
    // Join the pumps so the test harness never observes a lingering thread. The
    // reader unblocks promptly after teardown SIGKILLs the group (PTY closes).
    let _ = reader_thread.join();
    let _ = writer_thread.join();
}

/// Gracefully close the socket with the 4004 (unknown/ended session) code.
async fn close_unknown(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: CLOSE_UNKNOWN,
            reason: "unknown or ended session".into(),
        })))
        .await;
}

/// Apply one inbound TEXT control frame. Only `resize` is honoured; an `auth` frame
/// is tolerated (auth already ran at upgrade); anything else (incl. non-JSON) is
/// ignored (tolerant of unknown frames).
async fn apply_control(text: &str, session: &Arc<Mutex<Session>>) {
    let Ok(payload) = serde_json::from_str::<Value>(text) else {
        return; // malformed → ignored
    };
    let Some(t) = payload.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    if t == "resize" {
        // Tolerate missing/non-numeric fields
        // (defaults 24/80, errors swallowed).
        let rows = payload
            .get("rows")
            .and_then(|v| v.as_u64())
            .unwrap_or(24)
            .clamp(1, u16::MAX as u64) as u16;
        let cols = payload
            .get("cols")
            .and_then(|v| v.as_u64())
            .unwrap_or(80)
            .clamp(1, u16::MAX as u64) as u16;
        let _ = session.lock().await.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
    // "auth" + unknown types: ignored.
}

/// Remove `id` from the registry only if it still points at `target` (Arc identity).
/// Prevents a finishing relay from nuking a freshly-recreated session sharing the id.
async fn remove_if_eq(registry: &TerminalRegistry, id: &str, target: &Arc<Mutex<Session>>) -> bool {
    let mut map = registry.sessions.lock().await;
    if map.get(id).is_some_and(|s| Arc::ptr_eq(s, target)) {
        map.remove(id);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// First value of a request header, or `None` (case-insensitive lookup).
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

/// Opaque session id when no `X-Session-Id` is supplied
/// (`uuid4()[:8]` equivalent). We mix a monotonic counter with the current time into 8 hex
/// digits — process-unique without pulling in a randomness crate.
fn random_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mix = n.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(t);
    format!("{:08x}", mix & 0xFFFF_FFFF)
}

/// `datetime.now(timezone.utc).isoformat()` with a `Z` suffix, hand-rolled from
/// `SystemTime` (no `time`/`chrono` dep — keeps the audited tree minimal per D8).
/// Always emits 6 fractional digits: `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
fn now_iso_utc() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let micros = dur.subsec_micros();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{micros:06}Z")
}

/// Howard Hinnant's `civil_from_days` — pure arithmetic, no `unsafe`. Converts days
/// since the Unix epoch (1970-01-01) into a proleptic-Gregorian `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        // Unix epoch.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-02-29 (leap day) — 19_782 days after the Unix epoch.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        // 2026-01-01 — 20_454 days after the Unix epoch.
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
        // 2000-03-01 (the day after the 2000 leap day) — 11_017 days.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2025-06-15 — 20_254 days.
        assert_eq!(civil_from_days(20_254), (2025, 6, 15));
    }

    #[test]
    fn now_iso_utc_is_well_formed() {
        let s = now_iso_utc();
        assert!(s.ends_with('Z'), "{s}");
        // YYYY-MM-DDTHH:MM:SS.ffffffZ  (27 chars)
        assert_eq!(s.len(), 27, "{s}");
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
        assert_eq!(s.as_bytes()[26], b'Z');
    }

    #[test]
    fn random_ids_are_unique_and_hex() {
        let a = random_id();
        let b = random_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
