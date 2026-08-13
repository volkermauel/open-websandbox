//! `POST /execute` — hardened sandbox command execution.
//!
//! Spawns the command in its own process group (`process_group(0)`, the safe
//! `std` equivalent of `start_new_session=True`) so a timeout can
//! `killpg(SIGKILL)` the WHOLE tree, not just the shell. Output is capped per
//! stream at `MAX_OUTPUT_BYTES` with the same truncation message.
//! Timeout → `exit_code=124`, `timed_out=true`, empty output. HTTP 200
//! even on non-zero exit (the exit code is in the body, not the status).
//!
//! `RLIMIT_NPROC` is applied once at boot (see `main.rs`) and inherited by
//! children; it is NOT set per-command.

#![forbid(unsafe_code)]

use std::process::Stdio;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::metrics::{EXECUTE_COMMANDS_TOTAL, EXECUTE_TIMEOUTS_TOTAL};
use crate::safe_path::request_base;
use crate::state::AppState;

/// `POST /execute` request body. `timeout` is clamped into `[1, MAX_TIMEOUT]`,
/// defaulting to `DEFAULT_TIMEOUT` (seconds).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ExecuteRequest {
    /// Shell command line to execute.
    pub command: String,
    /// Optional wall-clock timeout in seconds (clamped into `[1, MAX_TIMEOUT]`).
    pub timeout: Option<u64>,
}

/// `POST /execute` response body (HTTP 200 even on non-zero exit).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ExecuteResponse {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code (HTTP 200 even when non-zero; `124` on timeout).
    pub exit_code: i32,
    /// Whether the command was killed for exceeding `timeout`.
    pub timed_out: bool,
}

/// Truncate output to `max` code points with the `_cap` message.
///
/// Mirrors `len(s)` semantics (`str` length = code points) so the
/// truncation byte count matches the runtime for the test output.
pub(crate) fn cap(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count > max {
        let head: String = s.chars().take(max).collect();
        format!("{head}\n...[truncated: {n} more bytes]\n", n = count - max)
    } else {
        s.to_string()
    }
}

/// The `/execute` handler.
///
/// `Authed` authenticates the per-session key; `HeaderMap` carries the optional
/// `X-Workspace-Subdir`; `Json` parses the body (body-consuming extractor last).
#[utoipa::path(
    post,
    path = "/execute",
    tag = "execute",
    request_body = ExecuteRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Captured stdout/stderr + exit code (HTTP 200 even on non-zero exit; timeout → exit_code 124)", body = ExecuteResponse),
        (status = 401, description = "Missing/invalid per-session Bearer", body = shared::ErrorResponse),
        (status = 500, description = "Failed to reap the command", body = shared::ErrorResponse)
    )
)]
pub async fn execute(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, ApiError> {
    let subdir = subdir_from(&headers);
    let base = request_base(&state.config.workdir, subdir)?;
    run_command(&state, &base, &req.command, req.timeout)
        .await
        .map(Json)
}

fn subdir_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-workspace-subdir")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

/// Core spawn/wait/kill logic, independent of HTTP wiring so it is unit-testable.
///
/// `child` is retained (only its piped stdout/stderr handles are read out) so we
/// can `.wait()` it in both the normal-exit and timeout paths and avoid leaving a
/// zombie (`_kill_group` + `proc.wait()` reap).
pub(crate) async fn run_command(
    state: &AppState,
    base: &std::path::Path,
    command: &str,
    timeout: Option<u64>,
) -> Result<ExecuteResponse, ApiError> {
    let cfg = &state.config;
    let timeout_secs = timeout
        .unwrap_or(cfg.default_timeout)
        .clamp(1, cfg.max_timeout);
    let preview: String = command.chars().take(200).collect();
    tracing::info!("exec (timeout={timeout_secs}s): {preview}");
    // D9: count every executed command (timeouts recorded separately).
    metrics::counter!(EXECUTE_COMMANDS_TOTAL).increment(1);

    let mut cmd = tokio::process::Command::new(&cfg.shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group -> pgid == child pid -> `killpg(pid)` tree-kills.
        .process_group(0)
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(ExecuteResponse {
                stdout: String::new(),
                stderr: format!("runtime error: {e}"),
                exit_code: 1,
                timed_out: false,
            });
        }
    };
    let pid = child.id().unwrap_or(0) as i32;
    let mut stdout_h = child.stdout.take();
    let mut stderr_h = child.stderr.take();
    let max = cfg.max_output_bytes;

    // Read both streams to EOF concurrently. EOF happens once every process that
    // inherited the pipe write-ends exits (for `echo`, immediately; for
    // backgrounded children, only after they die). We then `child.wait()` to reap.
    let read = async { tokio::join!(read_opt(&mut stdout_h), read_opt(&mut stderr_h)) };

    if let Ok((out_bytes, err_bytes)) =
        tokio::time::timeout(Duration::from_secs(timeout_secs), read).await
    {
        let status = child
            .wait()
            .await
            .map_err(|e| ApiError::Internal(format!("failed to reap command: {e}")))?;
        let stdout = out_bytes
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let stderr = err_bytes
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        Ok(ExecuteResponse {
            stdout: cap(&stdout, max),
            stderr: cap(&stderr, max),
            exit_code: status.code().unwrap_or(0),
            timed_out: false,
        })
    } else {
        // D9: this run hit the /execute timeout.
        metrics::counter!(EXECUTE_TIMEOUTS_TOTAL).increment(1);
        // Timeout: SIGKILL the whole process group, reap the direct child.
        if pid > 0 {
            // Best-effort; ESRCH (already gone) is ignored.
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = child.wait().await;
        Ok(ExecuteResponse {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 124,
            timed_out: true,
        })
    }
}

/// Read an optional piped stream to EOF, returning `None` on read error or
/// when the stream was absent.
async fn read_opt<R>(stream: &mut Option<R>) -> Option<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match stream {
        Some(r) => {
            let mut buf = Vec::new();
            r.read_to_end(&mut buf).await.ok()?;
            Some(buf)
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_leaves_short_output_alone() {
        assert_eq!(cap("hello", 32), "hello");
        // Exactly max is NOT truncated.
        assert_eq!(cap(&"X".repeat(32), 32).len(), 32);
    }

    #[test]
    fn cap_truncates_with_message() {
        let s = "X".repeat(2000);
        let out = cap(&s, 32);
        assert!(out.contains("...[truncated:"), "{out}");
        assert!(out.contains("1968 more bytes"), "{out}");
        assert!(out.len() < 2000);
    }
}
