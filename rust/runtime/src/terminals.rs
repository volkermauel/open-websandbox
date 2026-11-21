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
//!   dead sessions, **reuses a live session carrying the same id** (issue #129: the
//!   broker's reconnect path resumes that shell instead of killing it), and answers
//!   **429** once `MAX_TERMINAL_SESSIONS` live sessions are reached (resuming an
//!   existing session consumes no new slot, so it bypasses the cap); **503** on PTY
//!   spawn failure. A session whose id has a flushed scrollback file under the
//!   workspace is created with that tail preloaded (see *SIGTERM flush* below).
//! * `GET /api/terminals` (Authed) → `200 [{"id","created_at","pid"}, …]`, reaping
//!   any dead entries it observes.
//! * `GET /api/terminals/{id}` (Authed) → `200 {"id","created_at","pid"}` / **404**.
//! * `DELETE /api/terminals/{id}` (Authed) → `200 {"status":"deleted"}` (idempotent;
//!   also removes any flushed scrollback file so a later session with the same id
//!   starts clean).
//! * `GET /api/terminals/user/output` + `POST /api/terminals/user/input` (Authed)
//!   — the open-terminal *user shell* surface: read recent output from / inject
//!   text into the active-or-last-used session (does NOT create a shell; 404
//!   when none exists, 409 when it has disconnected).
//! * `GET /api/terminals/{id}` WebSocket (Authed at upgrade — 401 before the socket
//!   is accepted): **1:1 binary/text frames** relayed byte-for-byte.
//!   Inbound **binary** frames are raw stdin to the PTY; inbound **text** frames are
//!   JSON control messages (`{"type":"resize","rows":N,"cols":M}`, tolerated
//!   `{"type":"auth",…}`); PTY output is relayed back as **binary** frames. An
//!   unknown/dead session is closed with code **4004**; a second concurrent attach
//!   to a live session is closed with **4009**. On attach the scrollback tail is
//!   replayed first (one binary frame), so a reattached client sees recent output.
//!
//! # Stage-3 protocol (open-terminal 0.17.x / OWUI 0.13.x XTerminal)
//!
//! The first TEXT frame (forwarded by the broker from OWUI's auth message) may
//! carry `resume: true`, `cursor: <byte offset>`, `flow_control: true` and
//! `process_info: true`:
//!
//! * **Resume** — replay the retained history from `cursor` as an
//!   `{"type":"output","start","end","truncated"}` frame followed by one binary
//!   chunk covering `[start,end)`. `cursor` is an ABSOLUTE byte offset into the
//!   session's total output (`output_offset`); a cursor below the ring's eviction
//!   boundary replays from the boundary with `truncated: true`.
//! * **Flow control** — confirmed with `{"type":"flow_control"}`; the relay pauses
//!   output whenever `cursor - acknowledged >= FLOW_WINDOW` (client `ack{cursor}`
//!   frames release more). The window equals the scrollback cap so unacked bytes
//!   always remain in the ring (upstream: 256 KiB window / larger history —
//!   documented divergence). Without the confirm the client renders raw binary.
//! * **Process titles** — `process_info` enables `{"type":"process","pid","name"}`
//!   frames on foreground-process change (sampled ≤ 2×/s from
//!   `/proc/<shell>/stat` field 8 → `/proc/<pgrp>/comm`), driving OWUI's tab
//!   titles ("named after the running program").
//! * **Chat binding** — a `chat{chat_id}` frame binds the session; the session
//!   echoes `{"type":"chat","chat_id"}` or errors `Terminal already belongs to
//!   another chat` (a sandbox pod IS one chat's machine — cross-chat rebind is
//!   refused, matching upstream's reassignment rule).
//! * `ping` frames are tolerated (ignored).
//!
//! Clients that skip the hello (legacy paths) get the pre-stage-3 behavior:
//! raw ring replay + unbounded raw binary frames.
//!
//! # Session lifecycle — detach & resume (issue #129)
//!
//! The PTY outlives any single WS connection: a **client-side disconnect detaches**
//! (output keeps draining into the bounded scrollback ring; input stops) and a later
//! attach to the same id **resumes** the same shell. The PTY ends only when the shell
//! exits, `DELETE` runs, or the idle-detached sweep reaps a session no client has
//! reattached to within `TERMINAL_DETACH_TTL_SECS`. This is what lets a terminal
//! survive broker restarts and network blips — and, combined with the SIGTERM flush
//! below, node drain (new pod, same PVC, replayed tail).
//!
//! # SIGTERM flush (issue #129)
//!
//! On SIGTERM (pod eviction / drain) the runtime writes every live session's
//! scrollback ring to `<workdir>/.open-websandbox/scrollback/<id>.log` before
//! exiting. For persistent sandboxes the workspace volume is the per-user RWX PVC,
//! so the tail survives pod death and preloads the recreated session; for ephemeral
//! ones (emptyDir) the write is harmless and dies with the pod. Process state is
//! never preserved — only output.
//!
//! # Clean shutdown (no zombie)
//!
//! Each `Session` owns the `portable-pty` master + child plus two permanent pump
//! threads (stdin writer, output reader) for its whole life. `portable-pty` spawns
//! the shell with `setsid()`, so the shell is its own session/process-group leader
//! (pgid == pid). On teardown (shell exit, `DELETE`, dead-session reap) we
//! `killpg(SIGKILL)` the whole group and `wait()` to reap, so no zombie shell
//! survives; the pump threads self-terminate once the child dies (the master read
//! returns EOF/EIO).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

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
use tokio::sync::{mpsc, Mutex};
use tokio::task::spawn_blocking;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::request_base;
use crate::state::AppState;

/// WebSocket close code for an unknown or already-ended session.
const CLOSE_UNKNOWN: u16 = 4004;
/// How long the relay waits for the stage-3 hello before falling back to the
/// legacy raw-replay path. The broker forwards OWUI's auth frame immediately
/// after its own handshake, so a hello-connected client never notices; a
/// legacy/direct client pays one second before its raw tail replay.
const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
/// WebSocket close code for a live session that already has an attached client.
const CLOSE_BUSY: u16 = 4009;
/// Interval between idle-detached sweeps (see [`spawn_detached_sweep`]).
const SWEEP_INTERVAL: Duration = Duration::from_mins(1);
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
    /// Build an empty terminal-session registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Bounded tail of recent PTY output, replayed to a reattaching client and
/// flushed to the workspace on SIGTERM. Appended by the permanent reader pump.
/// `total` counts every byte EVER pushed (the absolute end offset the stage-3
/// resume cursor is measured against) — updated under the same lock as the
/// buffer, so `(total, snapshot)` reads are always consistent.
struct Scrollback {
    buf: Vec<u8>,
    cap: usize,
    total: u64,
}

impl Scrollback {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap,
            total: 0,
        }
    }

    /// Append `bytes`, evicting the OLDEST once `cap` is exceeded (the tail is
    /// what a reattaching client needs). `cap == 0` disables capture.
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        if self.cap == 0 || bytes.is_empty() {
            return;
        }
        self.buf.extend_from_slice(bytes);
        let excess = self.buf.len().saturating_sub(self.cap);
        if excess > 0 {
            self.buf.drain(..excess);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.clone()
    }
}

/// One live PTY session. Shared between the registry and the (single) active WS
/// relay via `Arc<Mutex<Session>>`; cheap brief locks guard every operation.
struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    created_at: String,
    pid: u32,
    /// Scrollback tail — appended by the reader pump, replayed on attach.
    /// `ring.total` is the absolute end byte offset for stage-3 resume.
    ring: Arc<StdMutex<Scrollback>>,
    /// The client's last `ack{cursor}` (stage-3 flow control) — updated by the
    /// control path, read by the relay's output gate.
    acknowledged: Arc<StdMutex<u64>>,
    /// Wakes the output gate on a fresh `ack`.
    ack_notify: Arc<tokio::sync::Notify>,
    /// Chat binding (`chat{chat_id}` frames; seeded from POST X-Session-Id).
    bound_chat: StdMutex<Option<String>>,
    /// Last user-visible activity — attach + `activity` frames select the
    /// "active or last-used" shell for `/api/terminals/user/*`.
    last_activity: StdMutex<Instant>,
    /// Whether an OWUI tab currently reports this shell as its active tab.
    active: StdMutex<bool>,
    /// Sink of the attached WS relay (`None` while detached).
    out_tx: Arc<StdMutex<Option<mpsc::Sender<Vec<u8>>>>>,
    /// Feed of the permanent stdin pump — cloned per attached relay.
    stdin_tx: mpsc::Sender<Vec<u8>>,
    /// When the WS client last detached (drives the idle-detached sweep).
    detached_since: StdMutex<Option<Instant>>,
}

impl Drop for Session {
    // reason: `pid` is a small positive `u32` from portable-pty; it always fits an `i32`.
    #[allow(clippy::cast_possible_wrap)]
    fn drop(&mut self) {
        // Safety net only: every normal teardown path (`teardown_one`) SIGKILLs the
        // group and `wait()`s to reap. If a `Session` is ever dropped without that,
        // best-effort `killpg` + a non-blocking `try_wait` keeps a shell from
        // outliving the process; the explicit reap in `teardown_one` is authoritative.
        let _ = killpg(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
        let _ = self.child.try_wait();
    }
}

/// Public info for a terminal session — the `GET /api/terminals` JSON body.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct TermInfo {
    id: String,
    created_at: String,
    pid: u32,
}

/// Response body for `POST /api/terminals` (id, created_at, pid).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateResponse {
    id: String,
    created_at: String,
    pid: u32,
}

/// Response body for `DELETE /api/terminals/{id}`.
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
// reason: `pid` is a small positive `u32` from portable-pty; it always fits an `i32`.
#[allow(clippy::cast_possible_wrap)]
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
///
/// # Errors
///
/// Returns [`ApiError`] (via [`request_base`]) for an invalid workspace
/// subdir, [`ApiError::TooManyRequests`] when the session cap is reached, and
/// [`ApiError::ServiceUnavailable`] if the PTY shell fails to spawn.
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
    // Resume marker: only the broker's PTY-resume probe (ensure_pty) may reuse
    // an existing session. Open WebUI creates one terminal per tab and sends
    // X-Session-Id: <chat id> — keying the new PTY on that would make a second
    // tab in the same chat attach to the first tab's PTY and be closed with
    // 4009 ("terminal already attached"; upstream open-terminal 0.16.1 has the
    // same one-attach policy). Upstream POST /api/terminals ignores
    // x-session-id for identity and generates a fresh id per request; honour
    // the same contract, with resume as an explicit opt-in.
    let resume = header_str(&headers, "x-terminal-resume") == Some("1") && session_id.is_some();
    let base = request_base(&state.config.workdir, subdir)?;
    // 0.17.7 start dir: new shells open there (inside the effective base only).
    let base = state
        .start_dir
        .as_deref()
        .filter(|d| d.starts_with(&base))
        .map_or_else(|| base.clone(), std::path::Path::to_path_buf);

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

        let id = match (resume, session_id.clone()) {
            (true, Some(id)) => id,
            _ => random_id(),
        };

        // D12 resume (issue #129): REUSE a live session carrying this id — a
        // reconnecting client (the broker's `ensure_pty`) is resuming that
        // shell, not replacing it. Only a dead leftover (a child that died
        // after the sweep above) is torn down and recreated.
        let reuse: Option<CreateResponse> = match map.get(&id).cloned() {
            Some(existing) if is_alive(&existing).await => {
                let g = existing.lock().await;
                Some(CreateResponse {
                    id: id.clone(),
                    created_at: g.created_at.clone(),
                    pid: g.pid,
                })
            }
            Some(_) => {
                if let Some(s) = map.remove(&id) {
                    to_teardown.push(s);
                }
                None
            }
            None => None,
        };

        if let Some(resp) = reuse {
            resp
        } else {
            // Cap is checked AFTER reaping; resuming an existing live
            // session consumes no new slot, so it bypasses the cap.
            if map.len() >= cap as usize {
                drop(map);
                for s in to_teardown {
                    teardown_one(s).await;
                }
                return Err(ApiError::TooManyRequests(format!(
                    "max {cap} terminals reached"
                )));
            }

            // Scrollback replay across pod generations (issue #129): the
            // SIGTERM flush persisted this id's output tail under the
            // workspace; preload it so the first attach replays it.
            let replay = read_scrollback(
                &state.config.workdir,
                &id,
                state.config.terminal_scrollback_bytes,
            );
            let session = match spawn_pty(
                &state.config.shell,
                &base,
                state.config.terminal_scrollback_bytes,
                &replay,
            ) {
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
            // Stage-3 chat binding: seed from X-Session-Id (the chat id) so a
            // `chat{chat_id}` frame matching it is idempotent and a DIFFERENT
            // chat is refused — a sandbox pod is one chat's machine.
            if let Some(chat) = &session_id {
                if let Ok(mut bound) = session.bound_chat.lock() {
                    *bound = Some(chat.clone());
                }
            }
            let resp = CreateResponse {
                id: id.clone(),
                created_at: session.created_at.clone(),
                pid: session.pid,
            };
            map.insert(id, Arc::new(Mutex::new(session)));
            resp
        }
    };

    for s in to_teardown {
        teardown_one(s).await;
    }
    Ok(Json(resp))
}

/// Build a PTY `Session` (24×80, `$SHELL`, `cwd`, `TERM=xterm-256color`) plus its
/// two PERMANENT pump threads — the master writer is single-take (portable-pty),
/// so one thread owns it for the session's whole life and input is fed via a
/// channel; the reader drains forever, into the scrollback ring while detached
/// and on to the attached relay otherwise. `replay` preloads the ring (issue #129:
/// the SIGTERM flush from a previous pod generation).
fn spawn_pty(
    shell: &str,
    cwd: &Path,
    scrollback_cap: usize,
    replay: &[u8],
) -> Result<Session, String> {
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

    // Scrollback ring — preloaded with any flushed tail from a previous pod
    // generation so the first attach replays it.
    let ring = Arc::new(StdMutex::new(Scrollback::new(scrollback_cap)));
    ring.lock().expect("scrollback lock").push(replay);

    // Permanent stdin pump: owns the once-only master writer; fed by `stdin_tx`.
    // A bounded channel applies backpressure — if the shell stops reading, the
    // relay stops draining the socket.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(IN_CHAN);
    let writer = master.take_writer().map_err(|e| format!("{e}"))?;
    std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(bytes) = stdin_rx.blocking_recv() {
            // Best-effort; a closed PTY surfaces as an error we simply drop.
            let _ = writer.write_all(&bytes);
        }
        // stdin_tx dropped (session teardown) → writer drops → EOF to the slave.
    });

    // Permanent output pump: drains the master forever. While a relay is
    // attached it forwards each chunk (blocking send = PTY backpressure, as
    // before); while detached it keeps draining into the scrollback ring so the
    // shell never blocks on a terminal nobody is watching. Every chunk also
    // advances `output_offset` — the absolute byte cursor stage-3 resume keys
    // off.
    let reader = master.try_clone_reader().map_err(|e| format!("{e}"))?;
    let sink = Arc::new(StdMutex::new(None::<mpsc::Sender<Vec<u8>>>));
    let ring_out = Arc::clone(&ring);
    let sink_out = Arc::clone(&sink);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                // portable-pty maps the Linux EIO-on-slave-close to Ok(0), so EOF
                // and read errors both end the pump (shell exited).
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    if let Ok(mut r) = ring_out.lock() {
                        r.push(&chunk);
                    }
                    let relay = sink_out.lock().ok().and_then(|g| (*g).clone());
                    if let Some(tx) = relay {
                        if tx.blocking_send(chunk).is_err() {
                            // Relay vanished without detaching — clear the stale
                            // sink and keep draining into the ring.
                            if let Ok(mut g) = sink_out.lock() {
                                *g = None;
                            }
                        }
                    }
                }
            }
        }
        // Reader end (shell exit): an attached relay observes its channel close
        // and tears the session down; a detached one is reaped by the sweep.
    });

    Ok(Session {
        master,
        child,
        created_at: now_iso_utc(),
        pid,
        ring,
        acknowledged: Arc::new(StdMutex::new(0)),
        ack_notify: Arc::new(tokio::sync::Notify::new()),
        bound_chat: StdMutex::new(None),
        last_activity: StdMutex::new(Instant::now()),
        active: StdMutex::new(false),
        out_tx: sink,
        stdin_tx,
        detached_since: StdMutex::new(None),
    })
}

// ---------------------------------------------------------------------------
// GET /api/terminals
// ---------------------------------------------------------------------------

/// `GET /api/terminals` — list live sessions, reaping any dead ones observed.
///
/// # Errors
///
/// Currently always returns `Ok`; the `Result` keeps the handler signature
/// uniform with the other `/api/terminals` endpoints.
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
        // A killed terminal must not replay later: drop any flushed scrollback.
        if let Some(p) = scrollback_path(&state.config.workdir, &id) {
            let _ = std::fs::remove_file(p);
        }
    }
    Json(DeleteResponse { status: "deleted" })
}

// ---------------------------------------------------------------------------
// WebSocket relay
// ---------------------------------------------------------------------------

/// Bidirectional PTY↔WS relay for one connection — the CLIENT side of a session's
/// lifecycle. A client-side disconnect DETACHES (issue #129): the shell lives on,
/// output keeps draining into the scrollback ring, and a later attach to the same
/// id resumes it. The PTY side ending (shell exit / heartbeat-detected death)
/// still tears the session down.
///
/// Stage-3 hello: the broker forwards OWUI's auth message (token stripped — the
/// runtime authenticates at upgrade via the per-session Bearer) with the
/// `resume`/`cursor`/`flow_control`/`process_info` block. A client that sends
/// binary first (or nothing) gets the pre-stage-3 raw behavior.
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

    // Attach: exactly one live relay per session — a second concurrent attach
    // would split PTY output between two cloned readers, so it is rejected.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(OUT_CHAN);
    let stdin_tx = {
        let g = session.lock().await;
        if g.out_tx.lock().map_or(true, |s| s.is_some()) {
            drop(g);
            close_busy(&mut socket).await;
            return;
        }
        // Statement-position locks: the guards drop at the `;`, so the session
        // lock is free for the `stdin_tx` clone below (no if-let temp lifetime).
        *g.out_tx.lock().expect("out_tx lock") = Some(out_tx);
        *g.detached_since.lock().expect("detached_since lock") = None;
        g.stdin_tx.clone()
    };

    // --- stage-3 hello ------------------------------------------------------
    // Wait briefly for the forwarded auth text frame. A binary frame first
    // (legacy client keystrokes) is fed to the shell and disables the new
    // protocol; silence falls back to raw replay too.
    let hello = read_hello(&mut socket, &stdin_tx).await;

    // Attach bookkeeping: activity stamp (user-shell selection) + active flag;
    // clone the ack counter + notifier out of the session lock so the output
    // gate below never needs the async lock.
    let (ack_counter, ack_notify) = {
        let g = session.lock().await;
        *g.last_activity.lock().expect("last_activity lock") = Instant::now();
        (Arc::clone(&g.acknowledged), Arc::clone(&g.ack_notify))
    };

    let mut cursor: u64 = hello.cursor.unwrap_or(0);
    let mut acknowledged: u64 = cursor;
    // The flow window equals the scrollback cap so unacked bytes always remain
    // in the ring (no mid-stream `truncated` in the steady state — see module
    // docs for the upstream divergence). Cap 0 disables capture ⇒ no resume.
    let flow_window = u64::try_from(state.config.terminal_scrollback_bytes).unwrap_or(u64::MAX);
    let mut flow_on = false;

    // Replay: stage-3 clients get an `output{start,end,truncated}` frame plus
    // one binary chunk from the cursor; legacy clients get the raw tail.
    if hello.resume {
        let (start, data, truncated) = history_window(&session, cursor).await;
        let end = start + data.len() as u64;
        if !data.is_empty() {
            let frame = serde_json::json!({
                "type": "output", "start": start, "end": end, "truncated": truncated
            });
            if socket
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return detach_or_teardown(&state, &session, &id, true).await;
            }
            if socket
                .send(Message::Binary(axum::body::Bytes::from(data)))
                .await
                .is_err()
            {
                return detach_or_teardown(&state, &session, &id, true).await;
            }
            cursor = end;
        }
        // Flow control is confirmed AFTER the replay, exactly like upstream.
        if hello.flow_control && flow_window > 0 {
            flow_on = true;
            // The gate measures cursor vs the session's acknowledged counter —
            // seed it with the replayed end.
            if let Ok(mut a) = ack_counter.lock() {
                *a = cursor;
            }
            acknowledged = cursor;
            if socket
                .send(Message::Text(
                    serde_json::json!({ "type": "flow_control" })
                        .to_string()
                        .into(),
                ))
                .await
                .is_err()
            {
                return detach_or_teardown(&state, &session, &id, true).await;
            }
        }
    } else {
        // Legacy replay: one raw binary frame with the retained tail.
        let replay = {
            let g = session.lock().await;
            g.ring
                .lock()
                .map_or_else(|_| Vec::default(), |r| r.snapshot())
        };
        if !replay.is_empty()
            && socket
                .send(Message::Binary(axum::body::Bytes::from(replay)))
                .await
                .is_err()
        {
            return detach_or_teardown(&state, &session, &id, true).await;
        }
        // Everything emitted so far is consumed by this legacy client — keep
        // the cursor parked at the current end so a later resume works.
        cursor = {
            let g = session.lock().await;
            g.ring.lock().map_or(cursor, |r| r.total)
        };
        acknowledged = cursor;
        if let Ok(mut a) = ack_counter.lock() {
            *a = cursor;
        }
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    // The first interval tick fires immediately; skip it so we don't tear down a
    // healthy session before its first output.
    heartbeat.tick().await;
    // Foreground-process sampler (stage-3 `process_info`): ≤2×/s, frames on change.
    let mut proc_sample = tokio::time::interval(Duration::from_millis(500));
    proc_sample.tick().await; // skip the immediate tick
    let mut last_fg: Option<(u32, String)> = None;

    // Which side ended the loop — a gone CLIENT detaches; a gone PTY tears down.
    let mut client_gone = false;

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
                    // `ack` updates the session's acknowledged counter (the gate
                    // below re-reads it after the notify); `chat` may produce a
                    // reply frame to send back.
                    if let Some(reply) = apply_control(&text, &session).await {
                        if socket.send(Message::Text(reply.to_string().into())).await.is_err() {
                            client_gone = true;
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    client_gone = true;
                    break;
                }
                // Ping/Pong are auto-answered by axum; ignore everything else.
                _ => {}
            },
            // PTY → WS (stdout).
            chunk = out_rx.recv() => match chunk {
                Some(c) => {
                    if flow_on {
                        // Gate on the client's acks: never get further ahead
                        // than the window (the ring holds that much history).
                        while cursor.saturating_sub(acknowledged) >= flow_window {
                            let notified = ack_notify.notified();
                            acknowledged = ack_counter.lock().map_or(acknowledged, |a| *a);
                            if cursor.saturating_sub(acknowledged) < flow_window {
                                break;
                            }
                            notified.await;
                        }
                    }
                    let len = c.len() as u64;
                    if flow_on {
                        let frame = serde_json::json!({
                            "type": "output", "start": cursor,
                            "end": cursor + len, "truncated": false
                        });
                        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                            client_gone = true;
                            break;
                        }
                    }
                    if socket.send(Message::Binary(axum::body::Bytes::from(c))).await.is_err() {
                        client_gone = true;
                        break;
                    }
                    cursor += len;
                }
                None => break, // reader ended (shell exited → EOF/EIO)
            },
            // Foreground-process change → title frame (stage-3 process_info).
            _ = proc_sample.tick(), if hello.process_info => {
                let current = foreground_process(&session).await;
                if current != last_fg {
                    last_fg = current.clone();
                    if let Some((pid, name)) = &current {
                        let frame = serde_json::json!({
                            "type": "process", "pid": pid, "name": name
                        });
                        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                            client_gone = true;
                            break;
                        }
                    }
                }
            }
            // Heartbeat: catches a dead shell whose slave is still held open by a
            // backgrounded grandchild (no EOF reaches the master read).
            _ = heartbeat.tick() => {
                if !is_alive(&session).await {
                    break;
                }
            }
        }
    }

    detach_or_teardown(&state, &session, &id, client_gone).await;
}

/// Common exit path: a gone CLIENT detaches (shell lives on); anything else
/// tears the session down.
async fn detach_or_teardown(
    state: &AppState,
    session: &Arc<Mutex<Session>>,
    id: &str,
    client_gone: bool,
) {
    // Drop our receiver FIRST so a reader pump parked on a full channel
    // unblocks the instant we stop draining it — the caller already dropped
    // `out_rx` (moved into the loop scope); clearing the sink is enough.
    if client_gone && is_alive(session).await {
        // Detach (issue #129): the shell lives on — output keeps draining
        // into the scrollback and a reconnect to the same id resumes this PTY.
        let g = session.lock().await;
        *g.out_tx.lock().expect("out_tx lock") = None;
        *g.detached_since.lock().expect("detached_since lock") = Some(Instant::now());
    } else {
        // PTY gone (shell exit / heartbeat) or unknown failure: full teardown.
        remove_if_eq(&state.terminals, id, session).await;
        teardown_one(session.clone()).await;
    }
}

/// The stage-3 hello: the first TEXT frame's protocol fields. `cursor` is an
/// absolute byte offset into the session's total output.
#[derive(Debug, Default)]
struct Hello {
    resume: bool,
    cursor: Option<u64>,
    flow_control: bool,
    process_info: bool,
}

/// Read the first frame with a short timeout. A TEXT frame parses as the
/// forwarded auth payload (token ignored — auth ran at upgrade via the
/// per-session Bearer); a BINARY frame is legacy keystrokes (fed to the shell);
/// silence falls back to the legacy path. Returns the parsed hello + whether a
/// binary frame was consumed.
async fn read_hello(socket: &mut WebSocket, stdin_tx: &mpsc::Sender<Vec<u8>>) -> Hello {
    let first = tokio::time::timeout(HELLO_TIMEOUT, socket.recv()).await;
    match first {
        Ok(Some(Ok(Message::Text(text)))) => {
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
                return Hello::default();
            };
            // The broker forwards OWUI's auth frame; tolerate direct clients too.
            if payload.get("type").and_then(|v| v.as_str()) == Some("auth") {
                Hello {
                    resume: payload.get("resume").and_then(serde_json::Value::as_bool)
                        == Some(true),
                    cursor: payload.get("cursor").and_then(serde_json::Value::as_u64),
                    flow_control: payload
                        .get("flow_control")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true),
                    process_info: payload
                        .get("process_info")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true),
                }
            } else {
                Hello::default()
            }
        }
        Ok(Some(Ok(Message::Binary(bytes)))) => {
            // Legacy client: keystrokes arrived before any hello.
            let _ = stdin_tx.send(bytes.to_vec()).await;
            Hello::default()
        }
        _ => Hello::default(),
    }
}

/// The retained history window from an absolute cursor: `(start, bytes,
/// truncated)` — a cursor below the ring's eviction boundary replays from the
/// boundary with `truncated: true` (upstream's `[Earlier output is no longer
/// available]` contract). `(total, snapshot)` is read under ONE lock, so the
/// start computation is always consistent with the buffer.
async fn history_window(session: &Arc<Mutex<Session>>, cursor: u64) -> (u64, Vec<u8>, bool) {
    let g = session.lock().await;
    let (total, data) = g
        .ring
        .lock()
        .map_or((0, Vec::new()), |r| (r.total, r.snapshot()));
    let start = total.saturating_sub(data.len() as u64);
    let from = cursor.max(start) as usize;
    let clamped = from.min(data.len());
    let bytes = data[clamped..].to_vec();
    let truncated = cursor < start;
    (start + clamped as u64, bytes, truncated)
}

/// The PTY's foreground process: `/proc/<shell>/stat` field 8 is the kernel's
/// notion of the controlling terminal's foreground pgid — `tcgetpgrp(slave)`
/// parity without touching the master fd. The NAME comes from the group
/// leader's `/proc/<pgid>/comm`. `None` when either read fails (upstream
/// simply omits the frame then).
async fn foreground_process(session: &Arc<Mutex<Session>>) -> Option<(u32, String)> {
    let pid = session.lock().await.pid;
    let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
        .await
        .ok()?;
    // Field 8 (1-indexed: pid, comm, state, ppid, pgrp, session, tty_nr, tpgid)
    // sits after the parenthesised comm — split on the LAST ')'.
    let tail = stat.rsplit_once(')').map(|(_, t)| t)?;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // fields[0] is `state` (field 3), so tpgid is fields[5].
    let tpgid: u32 = fields.get(5)?.parse().ok()?;
    if tpgid == 0 {
        return None;
    }
    let comm = tokio::fs::read_to_string(format!("/proc/{tpgid}/comm"))
        .await
        .ok()?;
    Some((tpgid, comm.trim().to_string()))
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

/// Gracefully close the socket with the 4009 (already attached) code.
async fn close_busy(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: CLOSE_BUSY,
            reason: "terminal already attached".into(),
        })))
        .await;
}

// ---------------------------------------------------------------------------
// SIGTERM flush + idle-detached sweep (issue #129)
// ---------------------------------------------------------------------------

/// Path of the flushed scrollback file for `id`, or `None` when `id` could not
/// appear in a filesystem path safely (charset-restricted: the broker derives
/// ids from k8s object names, but the header is client-controlled).
fn scrollback_path(workdir: &Path, id: &str) -> Option<PathBuf> {
    let safe = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
        && !id.starts_with('.');
    safe.then(|| {
        workdir
            .join(".open-websandbox")
            .join("scrollback")
            .join(format!("{id}.log"))
    })
}

/// Read the flushed scrollback tail for `id` (best-effort, bounded to `cap`).
/// Empty when absent, unreadable, disabled (`cap == 0`), or the id is unsafe.
fn read_scrollback(workdir: &Path, id: &str, cap: usize) -> Vec<u8> {
    use std::io::{Seek, SeekFrom};
    if cap == 0 {
        return Vec::new();
    }
    let Some(path) = scrollback_path(workdir, id) else {
        return Vec::new();
    };
    let Ok(mut f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    // Only the tail `cap` bytes matter — seek past anything older.
    if len > cap as u64 && f.seek(SeekFrom::Start(len - cap as u64)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    buf.truncate(cap);
    buf
}

/// Write every live session's scrollback tail under the workspace — the SIGTERM
/// path (issue #129). For persistent sandboxes the workspace volume IS the RWX
/// PVC, so the tails survive pod death and preload the recreated sessions; for
/// ephemeral ones (emptyDir) the writes are harmless and die with the pod.
/// Bounded rings keep this fast enough to finish well inside the default 30s
/// termination grace period. Returns how many sessions were flushed.
pub async fn flush_scrollbacks(state: &AppState) -> usize {
    if state.config.terminal_scrollback_bytes == 0 {
        return 0;
    }
    let dir = state
        .config
        .workdir
        .join(".open-websandbox")
        .join("scrollback");
    if std::fs::create_dir_all(&dir).is_err() {
        tracing::warn!(dir = %dir.display(), "scrollback dir unavailable — flush skipped");
        return 0;
    }
    let map = state.terminals.sessions.lock().await;
    let mut written = 0usize;
    for (id, s) in map.iter() {
        let bytes = s
            .lock()
            .await
            .ring
            .lock()
            .map_or_else(|_| Vec::default(), |r| r.snapshot());
        if bytes.is_empty() {
            continue;
        }
        let Some(path) = scrollback_path(&state.config.workdir, id) else {
            continue;
        };
        match std::fs::write(&path, &bytes) {
            Ok(()) => written += 1,
            Err(e) => tracing::warn!(id = %id, error = %e, "scrollback flush failed"),
        }
    }
    written
}

/// Spawn the idle-detached sweep (issue #129): periodically reap sessions whose
/// WS client has been gone longer than `TERMINAL_DETACH_TTL_SECS` — without it,
/// a detached PTY would count against `MAX_TERMINAL_SESSIONS` forever.
pub fn spawn_detached_sweep(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.tick().await; // first tick is immediate — skip it
        loop {
            tick.tick().await;
            let reaped = sweep_once(&state).await;
            if reaped > 0 {
                tracing::info!(
                    count = reaped,
                    "idle-detached terminal sweep reaped sessions"
                );
            }
        }
    })
}

/// One sweep pass: remove + teardown every session detached for longer than the
/// TTL. Sessions are re-verified under the registry lock, so an attach racing
/// this sweep keeps its session (it would have cleared `detached_since`).
async fn sweep_once(state: &AppState) -> usize {
    let ttl = Duration::from_secs(state.config.terminal_detach_ttl_secs);
    let now = Instant::now();
    let mut expired: Vec<Arc<Mutex<Session>>> = Vec::new();
    {
        let mut map = state.terminals.sessions.lock().await;
        for id in map.keys().cloned().collect::<Vec<_>>() {
            let Some(s) = map.get(&id).cloned() else {
                continue;
            };
            let idle = s.lock().await.detached_since.lock().ok().and_then(|g| *g);
            if idle.is_some_and(|since| now.duration_since(since) > ttl) {
                map.remove(&id);
                expired.push(s);
            }
        }
    }
    let reaped = expired.len();
    // Teardown AFTER releasing the registry lock (same pattern as create).
    for s in expired {
        teardown_one(s).await;
    }
    reaped
}

/// Apply one inbound TEXT control frame. Honoured: `resize` (PTY size),
/// `ack{cursor}` (stage-3 flow-control credit — updates the session counter and
/// wakes the output gate), `activity{active}` (tab visibility → last_activity
/// stamp + active flag for the user-shell surface), `chat{chat_id}` (chat
/// binding — returns the reply frame to send). Tolerated-and-ignored: `auth`
/// (auth ran at upgrade), `ping` keepalives, anything non-JSON.
async fn apply_control(text: &str, session: &Arc<Mutex<Session>>) -> Option<serde_json::Value> {
    let payload = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let t = payload.get("type").and_then(|v| v.as_str())?;
    match t {
        "resize" => {
            // Tolerate missing/non-numeric fields
            // (defaults 24/80, errors swallowed).
            let rows = payload
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(24)
                .clamp(1, u64::from(u16::MAX));
            let cols = payload
                .get("cols")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(80)
                .clamp(1, u64::from(u16::MAX));
            // reason: `rows`/`cols` are clamped to `u16::MAX` just above, so the
            // narrowing `u64`→`u16` cast never truncates a real value.
            #[allow(clippy::cast_possible_truncation)]
            let _ = session.lock().await.master.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
            None
        }
        "ack" => {
            let cursor = payload.get("cursor").and_then(serde_json::Value::as_u64)?;
            let g = session.lock().await;
            if let Ok(mut a) = g.acknowledged.lock() {
                *a = cursor;
            }
            g.ack_notify.notify_waiters();
            None
        }
        "activity" => {
            let active = payload.get("active").and_then(serde_json::Value::as_bool)?;
            let g = session.lock().await;
            *g.last_activity.lock().expect("last_activity lock") = Instant::now();
            *g.active.lock().expect("active lock") = active;
            None
        }
        "chat" => {
            let chat_id = payload.get("chat_id").and_then(serde_json::Value::as_str)?;
            let g = session.lock().await;
            let bound = g.bound_chat.lock().expect("bound_chat lock").clone();
            match bound {
                Some(existing) if existing == chat_id => {
                    Some(serde_json::json!({ "type": "chat", "chat_id": chat_id }))
                }
                Some(_) => Some(serde_json::json!({
                    "type": "chat",
                    "error": "Terminal already belongs to another chat"
                })),
                None => {
                    *g.bound_chat.lock().expect("bound_chat lock") = Some(chat_id.to_string());
                    Some(serde_json::json!({ "type": "chat", "chat_id": chat_id }))
                }
            }
        }
        // "auth", "ping", unknown types: ignored.
        _ => None,
    }
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
// /api/terminals/user/* — the open-terminal user-shell surface (baseline)
// ---------------------------------------------------------------------------

/// The ACTIVE-or-last-used session (upstream `_user_terminal`): among live
/// sessions, the one an OWUI tab most recently reported activity for (attach
/// and `activity` frames stamp `last_activity`). Does NOT create a shell.
async fn user_terminal(state: &AppState) -> Option<Arc<Mutex<Session>>> {
    let map = state.terminals.sessions.lock().await;
    // Upstream ranks by (active, last_active) — an ACTIVE tab wins even with
    // an older stamp (review finding 17).
    let mut best: Option<(bool, std::time::SystemTime, Arc<Mutex<Session>>)> = None;
    for s in map.values().cloned().collect::<Vec<_>>() {
        if !is_alive(&s).await {
            continue;
        }
        let g = s.lock().await;
        let active = g.active.lock().is_ok_and(|a| *a);
        let stamp = g
            .last_activity
            .lock()
            .map(|t| std::time::SystemTime::now() - t.elapsed())
            .ok();
        drop(g);
        let Some(stamp) = stamp else { continue };
        let replace = best.as_ref().is_none_or(|(b_active, b_stamp, _)| {
            active && !*b_active || active == *b_active && stamp > *b_stamp
        });
        if replace {
            best = Some((active, stamp, s));
        }
    }
    best.map(|(_, _, s)| s)
}

/// `GET /api/terminals/user/output` — recent output from the user's active or
/// last-used shell (LLM tool surface; OWUI probes this to gate the
/// `read_user_terminal` tool). Does not create a shell.
///
/// # Errors
///
/// 404 when no live session exists.
#[utoipa::path(
    get,
    path = "/api/terminals/user/output",
    operation_id = "read_user_terminal",
    tag = "terminals",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Recent PTY output", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "No live user shell", body = shared::ErrorResponse)
    )
)]
pub async fn read_user_terminal(
    _auth: Authed,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(session) = user_terminal(&state).await else {
        return Err(ApiError::NotFound("No connected user shell".to_string()));
    };
    let output = {
        let g = session.lock().await;
        g.ring.lock().map_or_else(
            |_| String::default(),
            |r| String::from_utf8_lossy(&r.snapshot()).into_owned(),
        )
    };
    Ok(Json(serde_json::json!({ "output": output })))
}

/// `POST /api/terminals/user/input` — send literal text to the user's active
/// or last-used shell (LLM tool surface; newlines submit). Does not wait for
/// completion and does not create a shell.
///
/// # Errors
///
/// 404 with no live session, 409 when the shell has disconnected.
#[utoipa::path(
    post,
    path = "/api/terminals/user/input",
    operation_id = "send_user_terminal_input",
    tag = "terminals",
    request_body = crate::processes::InputRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Input written", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "No live user shell", body = shared::ErrorResponse),
        (status = 409, description = "User shell has disconnected", body = shared::ErrorResponse)
    )
)]
pub async fn send_user_terminal_input(
    _auth: Authed,
    State(state): State<AppState>,
    Json(req): Json<crate::processes::InputRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(session) = user_terminal(&state).await else {
        return Err(ApiError::NotFound("No connected user shell".to_string()));
    };
    let stdin_tx = {
        let g = session.lock().await;
        g.stdin_tx.clone()
    };
    match stdin_tx.send(req.input.into_bytes()).await {
        Ok(()) => Ok(Json(serde_json::json!({ "status": "ok" }))),
        Err(_) => Err(ApiError::Conflict(
            "User shell has disconnected".to_string(),
        )),
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
// reason: the high bits of the nanos timestamp are mixed into a session-id
// hash; truncation after ~584 years is harmless and does not affect uniqueness.
#[allow(clippy::cast_possible_truncation)]
fn random_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mix = n.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(t);
    format!("{:08x}", mix & 0xFFFF_FFFF)
}

/// `datetime.now(timezone.utc).isoformat()` with a `Z` suffix, hand-rolled from
/// `SystemTime` (no `time`/`chrono` dep — keeps the audited tree minimal per D8).
/// Always emits 6 fractional digits: `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
// reason: seconds since the Unix epoch fit in `i64` for all realistic dates
// (i64::MAX ≈ year 292 billion), so the widening cast cannot lose the sign.
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn now_iso_utc() -> String {
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
// reason: Hinnant's algorithm guarantees `m`∈[1,12] and `d`∈[1,31], so the
// `i64`→`u32` casts of those two values never truncate.
#[allow(clippy::cast_possible_truncation)]
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

    #[test]
    fn scrollback_ring_evicts_oldest_beyond_cap() {
        // The TAIL is what a reattaching client needs: once `cap` is exceeded
        // the oldest bytes are dropped, never the newest (#129/#142 invariant —
        // a pod that dies mid-flush leaves a bounded, newest-first tail).
        let mut ring = Scrollback::new(8);
        ring.push(b"abcdefghij"); // 10 bytes > cap 8 → keep the last 8
        assert_eq!(ring.snapshot(), b"cdefghij");
        ring.push(b"XY"); // one more eviction
        assert_eq!(ring.snapshot(), b"efghijXY");
    }

    #[test]
    fn scrollback_ring_disabled_at_cap_zero() {
        // cap == 0 disables capture entirely — nothing may be buffered.
        let mut ring = Scrollback::new(0);
        ring.push(b"data");
        assert_eq!(ring.snapshot(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn flush_scrollbacks_noop_when_disabled() {
        use crate::auth::SessionKeyStore;
        use crate::config::RuntimeConfig;
        use crate::state::AppState;

        // terminal_scrollback_bytes == 0 → flush must return early: no session
        // is written AND the reserved dir is not created (a purge-then-die pod
        // with scrollback disabled must not resurrect .open-websandbox/, which
        // is exactly the shape that broke #142's restore-if-empty gate).
        let dir = tempfile::TempDir::new().unwrap();
        let state = AppState::new(
            RuntimeConfig {
                workdir: dir.path().to_path_buf(),
                terminal_scrollback_bytes: 0,
                ..Default::default()
            },
            SessionKeyStore::new(dir.path().join("api-key")),
        );
        assert_eq!(flush_scrollbacks(&state).await, 0);
        assert!(
            !dir.path().join(".open-websandbox").exists(),
            "disabled flush must not create the scrollback dir"
        );
    }
}
