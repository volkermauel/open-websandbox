//! The open-terminal **background-process** surface — `POST/GET /execute`,
//! `GET|WS /execute/{id}/status`, `POST /execute/{id}/input`,
//! `DELETE /execute/{id}` and the `WS /execute` list stream.
//!
//! Stage 3 (open-terminal v0.17.x / OWUI 0.13.x caller contract): every
//! command becomes a tracked background process with a bounded in-memory JSONL
//! log. OWUI's processes dock (`GET /execute` + `WS`), git clone
//! (`POST /execute?wait=1&tail=20`) and incremental output reads
//! (`offset`/`tail`/`truncated`) parse these shapes **directly** — the old
//! `{stdout, stderr, exit_code, timed_out}` body is gone.
//!
//! # Wire contract (ported from upstream `open_terminal/main.py` + `utils/log.py`)
//!
//! * `POST /execute` spawns the command in its own process group, registers it,
//!   optionally waits (`?wait=` seconds, 0–300) for completion, and returns the
//!   log snapshot: `{id, command, cwd, status, exit_code, output:[{type,data}],
//!   truncated, next_offset}` (`exit_code`/`output` while running: `null`/[]).
//! * `GET /execute` → `[{id, command, status, exit_code}, …]` — `status` is
//!   `"running" | "done" | "killed"`; finished entries stay listed until the
//!   expiry sweep drops them (`_EXPIRY_SECONDS` = 300 s upstream parity).
//! * `GET /execute/{id}/status?offset=&tail=&wait=` → the snapshot window
//!   (same as POST minus `cwd`); `wait` blocks up to N seconds for completion.
//! * `POST /execute/{id}/input` → writes text to stdin (400 once exited);
//!   literal escape sequences (`\n`, `\x03`, …) are decoded like upstream's
//!   `unicode_escape` round-trip — LLMs emit them as literal text.
//! * `DELETE /execute/{id}?force=` → SIGTERM (SIGKILL with `force=true`),
//!   `{"status":"killed"}`; idempotent for already-finished ids.
//! * Output records are per LINE with the trailing newline retained
//!   (asyncio `readline` parity), each counting one offset. `truncated` is
//!   true when the requested offset predates log eviction or `tail` cut it.
//!
//! # Divergences (documented in docs/compatibility.md)
//!
//! * Logs are **bounded in memory** (8 MiB/process, oldest evicted) instead of
//!   upstream's 50 MiB rotating on-disk JSONL — a sandbox pod's processes are
//!   ephemeral and the log only feeds the dock/resume windows.
//! * The runtime-extension `timeout` body field (previous contract, used by
//!   the e2e suite) still kills the command at that deadline
//!   (`status="killed"`); upstream never auto-kills.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::metrics::{EXECUTE_COMMANDS_TOTAL, EXECUTE_TIMEOUTS_TOTAL};
use crate::safe_path::request_base;
use crate::state::AppState;
use crate::terminals::now_iso_utc;

/// Log budget per process, in bytes of retained records (see module docs).
const LOG_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// A single pending line above this size is flushed as its own record so a
/// newline-free stream cannot grow the buffer unbounded.
const MAX_PENDING_LINE_BYTES: usize = 1024 * 1024;
/// Finished processes leave the registry after this long (upstream
/// `_EXPIRY_SECONDS`; the dock drops them from its list at the same point).
const EXPIRY_SECONDS: u64 = 300;
/// Period between expiry sweeps (upstream `_maintenance_loop` cadence).
const SWEEP_INTERVAL: Duration = Duration::from_mins(1);
/// WebSocket close code for an unknown/expired process (upstream parity).
const CLOSE_UNKNOWN: u16 = 4004;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// In-process background-process registry, held in [`AppState`] behind an
/// `Arc`. One entry per tracked command (running or finished-but-unexpired).
#[derive(Default)]
pub struct ProcessRegistry {
    processes: StdMutex<HashMap<String, Arc<Process>>>,
    /// Fires on spawn / finish / expiry — the `WS /execute` list stream.
    changed: Notify,
}

impl ProcessRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Process status — the wire values of upstream `BackgroundProcess.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Still running.
    Running,
    /// Exited on its own.
    Done,
    /// Killed via `DELETE` or the runtime-extension timeout.
    Killed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Killed => "killed",
        }
    }
}

/// One output log record: a stream label plus one line (terminator retained).
struct LogRecord {
    kind: LogKind,
    data: String,
}

#[derive(Debug, Clone, Copy)]
enum LogKind {
    Stdout,
    Stderr,
}

impl LogKind {
    fn as_str(self) -> &'static str {
        match self {
            LogKind::Stdout => "stdout",
            LogKind::Stderr => "stderr",
        }
    }
}

/// Bounded in-memory log: the retained tail of records plus the absolute
/// offset of the first retained record (`base_offset`). Offsets count only
/// output records — the `start`/`end` bookkeeping records upstream writes to
/// its on-disk JSONL never surface on the wire and don't need a port.
struct BoundedLog {
    entries: VecDeque<LogRecord>,
    /// Absolute offset of `entries[0]` (records evicted before it).
    base_offset: u64,
    bytes: usize,
}

impl BoundedLog {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            base_offset: 0,
            bytes: 0,
        }
    }

    /// Append a record, evicting the OLDEST until the byte budget holds.
    /// A single record larger than the budget is kept alone (the newest record
    /// is always retained, matching the tail-window contract).
    fn push(&mut self, record: LogRecord) {
        let size = record.data.len();
        self.entries.push_back(record);
        self.bytes += size;
        while self.bytes > LOG_BUDGET_BYTES && self.entries.len() > 1 {
            if let Some(evicted) = self.entries.pop_front() {
                self.bytes -= evicted.data.len();
                self.base_offset += 1;
            }
        }
    }

    /// The read window (upstream `_build_log_response` semantics): records
    /// with absolute index `>= offset` (clamped to the eviction boundary),
    /// with the last `tail` cut deducted; `truncated` flags either cut.
    fn read_window(&self, offset: u64, tail: Option<usize>) -> (Vec<serde_json::Value>, u64, bool) {
        let total = self.base_offset + self.entries.len() as u64;
        let eff = offset.max(self.base_offset);
        let available = total.saturating_sub(eff);
        let skip = tail.map_or(0u64, |t| available.saturating_sub(t as u64));
        let truncated = offset < self.base_offset || skip > 0;
        let start_index = (eff - self.base_offset) as usize + skip as usize;
        let output: Vec<serde_json::Value> = self
            .entries
            .iter()
            .skip(start_index)
            .map(|r| json!({ "type": r.kind.as_str(), "data": r.data }))
            .collect();
        (output, total, truncated)
    }
}

/// One tracked command.
pub(crate) struct Process {
    id: String,
    command: String,
    status: StdMutex<Status>,
    exit_code: StdMutex<Option<i32>>,
    /// When the process left `running` (drives the expiry sweep).
    finished_at: StdMutex<Option<Instant>>,
    /// Fires on every output record and on completion (WS status stream + waits).
    changed: Notify,
    log: StdMutex<BoundedLog>,
    /// Serializes stdin writers; a tokio (async) mutex because writes await
    /// while holding it.
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    pid: u32,
}

impl Process {
    fn is_running(&self) -> bool {
        *self.status.lock().expect("status lock") == Status::Running
    }

    fn push_record(&self, kind: LogKind, data: String) {
        if let Ok(mut log) = self.log.lock() {
            log.push(LogRecord { kind, data });
        }
        self.changed.notify_waiters();
    }

    fn finalize(&self, exit_code: i32, killed: bool) {
        {
            let mut status = self.status.lock().expect("status lock");
            if killed {
                *status = Status::Killed;
            } else if *status == Status::Running {
                *status = Status::Done;
            }
            *self.exit_code.lock().expect("exit_code lock") = Some(exit_code);
            *self.finished_at.lock().expect("finished_at lock") = Some(Instant::now());
        }
        self.changed.notify_waiters();
    }

    /// SIGTERM (or SIGKILL) the whole process group (`pgid == pid`, from
    /// `process_group(0)`). ESRCH (already gone) is ignored.
    fn kill_group(&self, force: bool) {
        if self.pid > 0 {
            // reason: pids are small positive u32s; the i32 narrowing is safe.
            #[allow(clippy::cast_possible_wrap)]
            let pid = Pid::from_raw(self.pid as i32);
            let _ = killpg(
                pid,
                if force {
                    Signal::SIGKILL
                } else {
                    Signal::SIGTERM
                },
            );
        }
    }

    /// Mark the process `killed` and anchor the expiry clock (the `DELETE`
    /// handler path; the log task's reap fills the exit code afterwards).
    async fn mark_killed(&self) {
        if let Ok(mut status) = self.status.lock() {
            *status = Status::Killed;
        }
        if let Ok(mut finished) = self.finished_at.lock() {
            *finished = Some(Instant::now());
        }
        // Close stdin so the child sees EOF if it survives SIGTERM long enough.
        self.stdin.lock().await.take();
        self.changed.notify_waiters();
    }

    /// The read-window snapshot — `log_response` parity (`status` reads omit
    /// `cwd`; the POST handler adds it).
    fn snapshot(&self, offset: u64, tail: Option<usize>) -> serde_json::Value {
        let (output, next_offset, truncated) = {
            match self.log.lock() {
                Ok(log) => log.read_window(offset, tail),
                Err(_) => (Vec::new(), 0, false),
            }
        };
        let running = self.is_running();
        json!({
            "id": self.id,
            "command": self.command,
            "status": if running { "running" } else {
                self.status.lock().map_or("running", |s| s.as_str())
            },
            "exit_code": self.exit_code.lock().ok().and_then(|c| *c),
            "output": output,
            "truncated": truncated,
            "next_offset": next_offset,
        })
    }
}

// ---------------------------------------------------------------------------
// POST /execute
// ---------------------------------------------------------------------------

/// Request body for `POST /execute` (upstream `ExecRequest` + the documented
/// runtime-extension `timeout`).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ExecRequest {
    /// Shell command line to execute (chaining, pipes and redirections work).
    pub command: String,
    /// Working directory for the command (workspace-relative or absolute
    /// inside the workspace; defaults to the workspace base).
    pub cwd: Option<String>,
    /// Extra environment variables merged into the subprocess environment.
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Runtime extension (NOT upstream): wall-clock deadline in seconds after
    /// which the command's process group is SIGKILLed (`status="killed"`).
    /// Upstream never auto-kills; this keeps the pre-stage-3 e2e contract.
    pub timeout: Option<u64>,
}

/// Query parameters for `POST /execute`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExecQuery {
    /// Seconds to wait for completion before returning (0–300). The finished
    /// snapshot arrives inline when the command completes in time.
    pub wait: Option<f64>,
    /// Return only the last N output entries.
    pub tail: Option<usize>,
}

/// The `POST /execute` response: the log snapshot plus the resolved `cwd`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ExecResponse {
    id: String,
    command: String,
    #[schema(value_type = String)]
    cwd: PathBuf,
    status: String,
    exit_code: Option<i32>,
    output: Vec<serde_json::Value>,
    truncated: bool,
    next_offset: u64,
}

/// `POST /execute` — run a command as a tracked background process.
///
/// # Errors
///
/// Returns [`ApiError`] for an invalid workspace subdir or a `cwd` escape.
#[utoipa::path(
    post,
    path = "/execute",
    operation_id = "run_command",
    tag = "execute",
    params(ExecQuery),
    request_body = ExecRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Process registered; finished-inline when ?wait= covered the runtime", body = ExecResponse),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn execute(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(query): AxumQuery<ExecQuery>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let subdir = subdir_from(&headers);
    let base = request_base(&state.config.workdir, subdir)?;
    let cwd = match req.cwd.as_deref() {
        Some(c) if !c.trim().is_empty() => safe_path_rel(c, &base)?,
        // 0.17.7 `OPEN_TERMINAL_START_DIR`: new commands open there (only
        // honoured when it sits inside the effective base).
        _ => state
            .start_dir
            .as_deref()
            .filter(|d| d.starts_with(&base))
            .map_or_else(|| base.clone(), Path::to_path_buf),
    };
    metrics::counter!(EXECUTE_COMMANDS_TOTAL).increment(1);
    let preview: String = req.command.chars().take(200).collect();
    tracing::info!("exec (bg): {preview}");

    let mut cmd = tokio::process::Command::new(&state.config.shell);
    cmd.arg("-c")
        .arg(&req.command)
        .current_dir(&cwd)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| !filtered_env(k)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Own process group -> pgid == child pid -> `killpg(pid)` tree-kills.
        .process_group(0);
    if let Some(env) = &req.env {
        cmd.envs(env);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(ApiError::Internal(format!("spawn failed: {e}")));
        }
    };
    let pid = child.id().unwrap_or(0);
    let stdout_h = child.stdout.take();
    let stderr_h = child.stderr.take();
    let stdin_h = child.stdin.take();

    let id = new_process_id();
    let process = Arc::new(Process {
        id: id.clone(),
        command: req.command.clone(),
        status: StdMutex::new(Status::Running),
        exit_code: StdMutex::new(None),
        finished_at: StdMutex::new(None),
        changed: Notify::new(),
        log: StdMutex::new(BoundedLog::new()),
        stdin: tokio::sync::Mutex::new(stdin_h),
        pid,
    });

    // Runtime-extension deadline (see module docs): kill at the clamped
    // timeout when still running.
    if let Some(t) = req.timeout {
        let clamped = t.clamp(1, state.config.max_timeout);
        let weak = Arc::downgrade(&process);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(clamped)).await;
            let Some(process) = weak.upgrade() else {
                return;
            };
            if process.is_running() {
                metrics::counter!(EXECUTE_TIMEOUTS_TOTAL).increment(1);
                process.kill_group(true);
                // Upstream's kill path marks `killed` immediately; the log
                // task's reap then fills the (negative-signal) exit code.
                process.mark_killed().await;
            }
        });
    }

    // The log task: drain both streams to EOF, then reap + finalize (upstream
    // `log_process`). Registered BEFORE returning so output always lands.
    let _log_task = spawn_process_log_task(process.clone(), stdout_h, stderr_h, child);

    state.processes.register(process.clone());
    state.processes.changed.notify_waiters();

    // `?wait=`: block for completion up to the window (upstream wraps a shield
    // on the log task; ours waits on the finish flag — output-only changes
    // don't end the wait).
    if let Some(wait) = query.wait.filter(|w| *w >= 0.0) {
        let _ = tokio::time::timeout(
            Duration::from_secs_f64(wait.clamp(0.0, 300.0)),
            wait_finished(&process),
        )
        .await;
    }

    let mut snapshot = process.snapshot(0, query.tail.filter(|t| *t >= 1));
    snapshot["cwd"] = json!(cwd);
    Ok(Json(snapshot))
}

/// Resolve a caller-supplied `cwd` inside the workspace base (relative or
/// absolute-inside-base, [`safe_path`] semantics) — upstream
/// `fs.resolve_path(request.cwd, cwd=session_cwd)` parity.
fn safe_path_rel(cwd: &str, base: &Path) -> Result<PathBuf, ApiError> {
    crate::safe_path::safe_path(cwd, base)
}

fn subdir_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-workspace-subdir")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

/// Environment allow-list filter — commands, interactive terminals and
/// notebooks never inherit the runtime's own secrets (upstream 0.14.3
/// parity): the runtime key path and any `*_KEY`/`*SECRET*`/`*TOKEN*`/broker
/// variables are stripped from the child environment.
fn filtered_env(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.contains("KEY") || k.contains("SECRET") || k.contains("TOKEN") || k.contains("PASSWORD")
}

// ---------------------------------------------------------------------------
// Spawn helpers
// ---------------------------------------------------------------------------

/// Drain `stdout` + `stderr` into the process log, then reap + finalize.
fn spawn_process_log_task(
    process: Arc<Process>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    mut child: tokio::process::Child,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::join!(
            maybe_pump(stdout, LogKind::Stdout, &process),
            maybe_pump(stderr, LogKind::Stderr, &process)
        );
        let status = child.wait().await.ok();
        let exit_code = status.as_ref().map_or(0, |s| {
            // `code()` is None when the child died on a signal — report the
            // negated signal number (Python `returncode` parity, -9 for
            // SIGKILL) via the unix `ExitStatusExt` trait.
            use std::os::unix::process::ExitStatusExt;
            s.code().or_else(|| s.signal().map(|sig| -sig)).unwrap_or(0)
        });
        process.finalize(exit_code, false);
    })
}

/// Pump one piped stream line-by-line into the log (asyncio `readline` parity:
/// the trailing newline is retained; a newline-free remainder is flushed at
/// EOF; an over-long pending line is flushed early to bound memory).
async fn maybe_pump<R: AsyncRead + Unpin>(
    stream: Option<R>,
    kind: LogKind,
    process: &Arc<Process>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                pending.extend_from_slice(&chunk[..n]);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    process.push_record(kind, String::from_utf8_lossy(&line).into_owned());
                }
                if pending.len() > MAX_PENDING_LINE_BYTES {
                    let partial = std::mem::take(&mut pending);
                    process.push_record(kind, String::from_utf8_lossy(&partial).into_owned());
                }
            }
        }
    }
    if !pending.is_empty() {
        process.push_record(kind, String::from_utf8_lossy(&pending).into_owned());
    }
}

/// Resolve when the process has FINISHED REAPING (exit code set) — the
/// `?wait=`/status-wait contract. The `notified` future is created BEFORE the
/// check, so a concurrent completion can't be missed; `mark_killed` fires an
/// early notify that re-loops until the log task's reap lands the exit code.
async fn wait_finished(process: &Arc<Process>) {
    loop {
        let notified = process.changed.notified();
        let reaped = process.exit_code.lock().is_ok_and(|c| c.is_some());
        if reaped {
            return;
        }
        notified.await;
    }
}

/// `YYYYMMDD-HHMMSS-<6 hex>` — upstream's `process_id` format.
fn new_process_id() -> String {
    // `now_iso_utc` is `YYYY-MM-DDTHH:MM:SS.ffffffZ`; compact the date+time.
    let iso = now_iso_utc();
    let compact: String = iso.chars().filter(char::is_ascii_digit).collect();
    // YYYYMMDDHHMMSSffffff → take YYYYMMDD-HHMMSS-
    format!("{}-{}-{}", &compact[0..8], &compact[8..14], hex6())
}

/// Six hex digits without a randomness crate (same mix as the terminal ids).
// reason: the high bits of the nanos timestamp are mixed into a hash; the
// truncation is deliberate and collision-improbable within a pod.
#[allow(clippy::cast_possible_truncation)]
fn hex6() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mix = n.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(t);
    format!("{:06x}", mix & 0xFF_FFFF)
}

// ---------------------------------------------------------------------------
// GET /execute (list) + WS /execute (list stream)
// ---------------------------------------------------------------------------

/// `GET /execute` — list tracked processes (running + finished-unexpired).
///
/// # Errors
///
/// Always `Ok` (uniform handler signature).
#[utoipa::path(
    get,
    path = "/execute",
    operation_id = "list_processes",
    tag = "execute",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Tracked processes for this session", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn list_processes(
    _auth: Authed,
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    Ok(Json(state.processes.list()))
}

/// `GET /execute` WebSocket — the live process-list stream (same payload as
/// the plain GET, pushed on every change). Upgrades branch to
/// [`execute_list_ws`]; plain requests get [`list_processes`].
pub async fn execute_get_or_ws(
    auth: Authed,
    State(state): State<AppState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if let Ok(upgrade) = ws {
        return upgrade.on_upgrade(move |socket| list_stream(state, socket));
    }
    list_processes(auth, State(state)).await.into_response()
}

/// The list-stream pump: push the list, then wait for a registry change.
async fn list_stream(state: AppState, mut socket: WebSocket) {
    loop {
        let payload =
            serde_json::to_string(&state.processes.list()).unwrap_or_else(|_| "[]".into());
        if socket.send(Message::Text(payload.into())).await.is_err() {
            return;
        }
        // Notify semantics: register the waiter BEFORE re-checking nothing —
        // the next change (spawn/finish/expire) wakes us.
        state.processes.changed.notified().await;
    }
}

// ---------------------------------------------------------------------------
// GET /execute/{id}/status (REST + WS)
// ---------------------------------------------------------------------------

/// Query parameters for `GET /execute/{id}/status`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatusQuery {
    /// Return records after this absolute offset (per-reader cursor).
    pub offset: Option<u64>,
    /// Return only the last N output entries (1–1000, upstream bound).
    pub tail: Option<usize>,
    /// Seconds to wait for completion before answering.
    pub wait: Option<f64>,
}

/// `GET /execute/{id}/status` — process metadata + output window. WebSocket
/// upgrades stream the same payload on every change until completion.
///
/// # Errors
///
/// 404 when the id is unknown or expired.
#[utoipa::path(
    get,
    path = "/execute/{id}/status",
    operation_id = "get_process_status",
    tag = "execute",
    params(("id" = String, Path, description = "Process id"), StatusQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Process status + output window", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Process not found", body = shared::ErrorResponse)
    )
)]
pub async fn process_status(
    _auth: Authed,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    AxumQuery(query): AxumQuery<StatusQuery>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let process = state.processes.get(&id);
    if let Ok(upgrade) = ws {
        // Upstream accepts the upgrade, authenticates, then closes 4004
        // for an unknown/expired id — the client's retry loop stops there
        // instead of hammering a 404 handshake.
        let offset = query.offset.unwrap_or(0);
        let tail = query.tail;
        return upgrade.on_upgrade(move |socket| status_stream(process, offset, tail, socket));
    }
    let Some(process) = process else {
        return ApiError::NotFound("Process not found".to_string()).into_response();
    };
    // `wait`: block for completion up to N seconds (upstream parity).
    if let Some(wait) = query.wait.filter(|w| *w >= 0.0) {
        let _ = tokio::time::timeout(
            Duration::from_secs_f64(wait.clamp(0.0, 300.0)),
            wait_finished(&process),
        )
        .await;
    }
    let snapshot = process.snapshot(query.offset.unwrap_or(0), clamp_tail(query.tail));
    Json(snapshot).into_response()
}

/// Upstream bounds `tail` to 1..=1000 on the status endpoint.
fn clamp_tail(tail: Option<usize>) -> Option<usize> {
    tail.map(|t| t.clamp(1, 1000)).filter(|t| *t >= 1)
}

/// The status-stream pump: snapshot, wait for a change, stop at completion.
/// An unknown/expired id closes `4004` (process gone — no reconnect).
async fn status_stream(
    process: Option<Arc<Process>>,
    offset: u64,
    tail: Option<usize>,
    mut socket: WebSocket,
) {
    let Some(process) = process else {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_UNKNOWN,
                reason: "process not found".into(),
            })))
            .await;
        return;
    };
    loop {
        let snapshot = process.snapshot(offset, tail);
        let finished = !process.is_running();
        if socket
            .send(Message::Text(
                serde_json::to_string(&snapshot)
                    .unwrap_or_else(|_| "{}".into())
                    .into(),
            ))
            .await
            .is_err()
        {
            return;
        }
        if finished {
            return;
        }
        process.changed.notified().await;
    }
}

// ---------------------------------------------------------------------------
// POST /execute/{id}/input
// ---------------------------------------------------------------------------

/// Request body for `POST /execute/{id}/input` (upstream `InputRequest`).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct InputRequest {
    /// Text to write to stdin; include newlines to submit.
    pub input: String,
}

/// `POST /execute/{id}/input` — write text to a running process's stdin.
///
/// # Errors
///
/// 404 unknown id, 400 when the process already exited or its stdin closed.
#[utoipa::path(
    post,
    path = "/execute/{id}/input",
    operation_id = "send_process_input",
    tag = "execute",
    params(("id" = String, Path, description = "Process id")),
    request_body = InputRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Input written", body = serde_json::Value),
        (status = 400, description = "Process exited or stdin closed", body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Process not found", body = shared::ErrorResponse)
    )
)]
pub async fn send_input(
    _auth: Authed,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<InputRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(process) = state.processes.get(&id) else {
        return Err(ApiError::NotFound("Process not found".to_string()));
    };
    if !process.is_running() {
        return Err(ApiError::BadRequest(
            "Process has already exited".to_string(),
        ));
    }
    let text = decode_unicode_escape(&req.input);
    let bytes = text.into_bytes();
    // Serialize concurrent writers (upstream `input_lock`); the async mutex is
    // held across the bounded write.
    let mut stdin = process.stdin.lock().await;
    match stdin.as_mut() {
        Some(pipe) => {
            let write = pipe.write_all(&bytes);
            if tokio::time::timeout(Duration::from_secs(30), write)
                .await
                .is_err()
                || pipe.flush().await.is_err()
            {
                return Err(ApiError::BadRequest("Process stdin is closed".to_string()));
            }
            Ok(Json(json!({ "status": "ok" })))
        }
        None => Err(ApiError::BadRequest("Process stdin is closed".to_string())),
    }
}

/// Decode Python `unicode_escape`-style backslash sequences from literal LLM
/// text (upstream: `input.encode("raw_unicode_escape").decode("unicode_escape")`).
/// Non-ASCII characters pass through unchanged; `\xHH`/`\uHHHH` become the
/// encoded code points; an unknown escape keeps its backslash (Python parity).
fn decode_unicode_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('x') => take_hex(&mut chars, 2, &mut out),
            Some('u') => take_hex(&mut chars, 4, &mut out),
            Some('U') => take_hex(&mut chars, 8, &mut out),
            Some(other @ '0'..='7') => {
                // Octal: up to two more digits.
                let mut value = other.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.clone().next().and_then(|d| d.to_digit(8)) {
                        Some(d) => {
                            chars.next();
                            value = value * 8 + d;
                        }
                        None => break,
                    }
                }
                // Python maps the byte value through latin-1.
                out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Consume `n` hex digits and push the code point (invalid digits are kept
/// literally, mirroring Python's error-free lossiness).
fn take_hex(chars: &mut std::str::Chars<'_>, n: usize, out: &mut String) {
    let mut value: u32 = 0;
    for _ in 0..n {
        if let Some(d) = chars.clone().next().and_then(|c| c.to_digit(16)) {
            chars.next();
            value = value * 16 + d;
        } else {
            out.push('\u{fffd}');
            return;
        }
    }
    out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
}

// ---------------------------------------------------------------------------
// DELETE /execute/{id}
// ---------------------------------------------------------------------------

/// Query parameters for `DELETE /execute/{id}`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct KillQuery {
    /// Send SIGKILL instead of SIGTERM.
    pub force: Option<bool>,
}

/// `DELETE /execute/{id}` — terminate a running process (idempotent).
///
/// # Errors
///
/// 404 when the id is unknown or expired.
#[utoipa::path(
    delete,
    path = "/execute/{id}",
    operation_id = "kill_process",
    tag = "execute",
    params(("id" = String, Path, description = "Process id"), KillQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Kill signalled (idempotent)", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Process not found", body = shared::ErrorResponse)
    )
)]
pub async fn kill_process(
    _auth: Authed,
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    AxumQuery(query): AxumQuery<KillQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(process) = state.processes.get(&id) else {
        return Err(ApiError::NotFound("Process not found".to_string()));
    };
    if process.is_running() {
        process.kill_group(query.force.unwrap_or(false));
        // Mark killed immediately (upstream parity); the log task's reap
        // fills the exit code.
        process.mark_killed().await;
    }
    Ok(Json(json!({ "status": "killed" })))
}

// ---------------------------------------------------------------------------
// Registry impls + sweep
// ---------------------------------------------------------------------------

impl ProcessRegistry {
    fn register(&self, process: Arc<Process>) {
        if let Ok(mut map) = self.processes.lock() {
            map.insert(process.id.clone(), process);
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Process>> {
        self.processes.lock().ok()?.get(id).cloned()
    }

    /// The dock list, id-sorted (ids are time-prefixed ⇒ insertion order).
    fn list(&self) -> Vec<serde_json::Value> {
        let map = self.processes.lock().ok();
        let Some(map) = map else { return Vec::new() };
        let mut ids: Vec<&String> = map.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| {
                let p = map.get(id)?;
                Some(json!({
                    "id": p.id,
                    "command": p.command,
                    "status": p.status.lock().ok()?.as_str(),
                    "exit_code": p.exit_code.lock().ok().and_then(|c| *c),
                }))
            })
            .collect()
    }

    /// Drop finished processes older than [`EXPIRY_SECONDS`]; returns how many
    /// (the sweep notifies the list stream when non-zero).
    fn expire(&self) -> usize {
        let mut expired = Vec::new();
        if let Ok(mut map) = self.processes.lock() {
            map.retain(|_, p| {
                let finished = p.finished_at.lock().ok().and_then(|f| *f);
                match finished {
                    Some(at) => {
                        let keep = at.elapsed() <= Duration::from_secs(EXPIRY_SECONDS);
                        if !keep {
                            expired.push(p.pid);
                        }
                        keep
                    }
                    None => true,
                }
            });
        }
        let n = expired.len();
        if n > 0 {
            self.changed.notify_waiters();
        }
        n
    }
}

/// Periodic expiry sweep (spawn once at boot, like the terminal sweep).
pub fn spawn_process_sweep(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.tick().await; // first tick is immediate — skip it
        loop {
            tick.tick().await;
            let expired = state.processes.expire();
            if expired > 0 {
                tracing::info!(count = expired, "process expiry sweep reaped entries");
            }
        }
    })
}

/// SIGKILL every still-running process group — the SIGTERM shutdown path
/// (mirrors `terminals::flush_scrollbacks` ordering in `main.rs`).
pub async fn kill_all_running(state: &AppState) {
    let running: Vec<Arc<Process>> = state
        .processes
        .processes
        .lock()
        .ok()
        .map(|map| map.values().filter(|p| p.is_running()).cloned().collect())
        .unwrap_or_default();
    for p in running {
        p.kill_group(true);
        p.mark_killed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_window_offsets_and_truncation() {
        let mut log = BoundedLog::new();
        for i in 0..5 {
            log.push(LogRecord {
                kind: LogKind::Stdout,
                data: format!("line{i}\n"),
            });
        }
        // Full read: offset 0 → 5 records, next_offset 5, not truncated.
        let (out, next, truncated) = log.read_window(0, None);
        assert_eq!(out.len(), 5);
        assert_eq!(next, 5);
        assert!(!truncated);
        // Incremental read from offset 3 → lines 3+4.
        let (out, next, _) = log.read_window(3, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["data"], "line3\n");
        assert_eq!(next, 5);
        // tail=2 keeps only the LAST two of the available window.
        let (out, _, truncated) = log.read_window(0, Some(2));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["data"], "line3\n");
        assert!(truncated);
        // Reading BELOW the first retained record (after eviction) truncates.
        log.push(LogRecord {
            kind: LogKind::Stderr,
            data: "e\n".into(),
        });
        let (out, _, truncated) = log.read_window(0, None);
        assert_eq!(out.len(), 6);
        assert!(!truncated);
    }

    #[test]
    fn log_eviction_bumps_base_offset_and_flags_truncation() {
        let mut log = BoundedLog::new();
        // Shrink the budget's effect by pushing >LOG_BUDGET is impractical in a
        // test (8 MiB); simulate eviction by base_offset arithmetic instead:
        // push records, then force evictions via a tiny budget through direct
        // field manipulation.
        for i in 0..3 {
            log.push(LogRecord {
                kind: LogKind::Stdout,
                data: format!("l{i}\n"),
            });
        }
        // Simulate the eviction of the first record.
        if let Some(front) = log.entries.pop_front() {
            log.bytes -= front.data.len();
            log.base_offset += 1;
        }
        assert_eq!(log.base_offset, 1);
        // A client that never saw the eviction asks for offset 0 → served from
        // the boundary with truncated=true.
        let (out, total, truncated) = log.read_window(0, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["data"], "l1\n");
        assert_eq!(total, 3);
        assert!(truncated);
        // A client at the boundary sees the window without truncation.
        let (_, _, truncated) = log.read_window(1, None);
        assert!(!truncated);
    }

    #[test]
    fn escape_decoder_matches_python_semantics() {
        assert_eq!(decode_unicode_escape(r"ls -la\n"), "ls -la\n");
        assert_eq!(decode_unicode_escape("plain text"), "plain text");
        assert_eq!(decode_unicode_escape(r"a\tb"), "a\tb");
        assert_eq!(decode_unicode_escape(r"\x41\x42"), "AB");
        assert_eq!(decode_unicode_escape(r"\u00e9"), "é");
        // Unknown escapes keep the backslash (Python parity).
        assert_eq!(decode_unicode_escape(r"\q"), r"\q");
        // Non-ASCII passes through untouched.
        assert_eq!(decode_unicode_escape("héllo\n"), "héllo\n");
        // Ctrl-C as the LLM would send it.
        assert_eq!(decode_unicode_escape(r"\x03"), "\u{3}");
    }

    #[test]
    fn process_id_format_matches_upstream() {
        let id = new_process_id();
        // YYYYMMDD-HHMMSS-hexxx
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "{id}");
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 6);
        assert_eq!(parts[2].len(), 6);
        assert!(parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit())));
    }

    #[tokio::test]
    async fn process_lifecycle_spawns_waits_and_kills() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = AppState::new(
            crate::config::RuntimeConfig {
                workdir: dir.path().to_path_buf(),
                ..Default::default()
            },
            crate::auth::SessionKeyStore::new(dir.path().join("api-key")),
        );
        let base = dir.path().to_path_buf();

        // 1. Spawn a command that finishes quickly.
        let process = spawn_for_test(&state, &base, "echo hello", None);
        let _ = tokio::time::timeout(Duration::from_secs(5), wait_finished(&process)).await;
        assert_eq!(status_str(&process), "done");
        let snapshot = process.snapshot(0, None);
        assert_eq!(snapshot["exit_code"], 0);
        let output = snapshot["output"].as_array().unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "stdout");
        // asyncio readline parity: the trailing newline is RETAINED.
        assert_eq!(output[0]["data"], "hello\n");
        assert_eq!(snapshot["next_offset"], 1);
        assert!(!snapshot["truncated"].as_bool().unwrap());

        // 2. A running process: input + kill.
        let process = spawn_for_test(&state, &base, "cat", None);
        // Feed input via the stdin handle (handler-level path tested in e2e).
        {
            let mut stdin = process.stdin.lock().await;
            let pipe = stdin.as_mut().unwrap();
            pipe.write_all(b"ping\n").await.unwrap();
            pipe.flush().await.unwrap();
        }
        // Output eventually carries the echoed line.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = process.snapshot(0, None);
            if snapshot["output"].as_array().is_some_and(|o| !o.is_empty()) {
                assert_eq!(snapshot["output"][0]["data"], "ping\n");
                break;
            }
            assert!(Instant::now() < deadline, "no output within 5s");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Kill with SIGKILL (cat has no TERM handler guarantee), then mark
        // killed exactly like the DELETE handler.
        process.kill_group(true);
        process.mark_killed().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), wait_finished(&process)).await;
        assert_eq!(status_str(&process), "killed");
    }

    // --- test helpers ------------------------------------------------------

    fn spawn_for_test(
        state: &AppState,
        base: &Path,
        command: &str,
        env: Option<std::collections::BTreeMap<String, String>>,
    ) -> Arc<Process> {
        let mut cmd = tokio::process::Command::new(&state.config.shell);
        cmd.arg("-c")
            .arg(command)
            .current_dir(base)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        if let Some(env) = env {
            cmd.envs(env);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let process = Arc::new(Process {
            id: new_process_id(),
            command: command.to_string(),
            status: StdMutex::new(Status::Running),
            exit_code: StdMutex::new(None),
            finished_at: StdMutex::new(None),
            changed: Notify::new(),
            log: StdMutex::new(BoundedLog::new()),
            stdin: tokio::sync::Mutex::new(stdin),
            pid: child.id().unwrap_or(0),
        });
        spawn_process_log_task(process.clone(), stdout, stderr, child);
        state.processes.register(process.clone());
        process
    }

    fn status_str(process: &Process) -> &'static str {
        process.status.lock().unwrap().as_str()
    }
}
