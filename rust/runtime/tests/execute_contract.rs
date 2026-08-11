//! Execute-contract tests (9 cases).
//!
//! Real `tokio::process::Command` spawn — exit codes, stdout/stderr capture and
//! split, the `cap` truncation at the configured cap, the timeout path
//! (`exit_code=124`, `timed_out=true`), and the whole-process-group kill that
//! leaves no orphan behind. `/bin/sh` (dash) is the shell, so every command is
//! POSIX-portable (no bash-isms).

#![forbid(unsafe_code)]

mod common;

use std::time::Instant;

use axum::http::{Method, StatusCode};
use serde::Deserialize;

use common::Bearer;

#[derive(Deserialize)]
struct ExecOut {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

async fn exec(env: &common::Env, command: &str, timeout: Option<u64>) -> ExecOut {
    let body = match timeout {
        Some(t) => format!(
            r#"{{"command":{cmd},"timeout":{t}}}"#,
            cmd = serde_json::to_string(command).unwrap()
        ),
        None => format!(
            r#"{{"command":{cmd}}}"#,
            cmd = serde_json::to_string(command).unwrap()
        ),
    };
    let resp = env
        .send(Method::POST, "/execute", Bearer::Default, None, Some(body))
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "execute should always 200");
    common::json(resp).await
}

#[tokio::test]
async fn exit_code_nonzero() {
    let env = common::Env::new();
    let out = exec(&env, "exit 7", None).await;
    assert_eq!(out.exit_code, 7);
    assert!(!out.timed_out);
}

#[tokio::test]
async fn exit_code_zero() {
    let env = common::Env::new();
    let out = exec(&env, "true", None).await;
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn stdout_capture() {
    let env = common::Env::new();
    let out = exec(&env, "echo hello", None).await;
    assert_eq!(out.stdout, "hello\n");
    assert_eq!(out.stderr, "");
    assert_eq!(out.exit_code, 0);
}

#[tokio::test]
async fn stderr_split() {
    let env = common::Env::new();
    let out = exec(&env, "echo out; echo err 1>&2", None).await;
    assert!(out.stdout.contains("out") && !out.stdout.contains("err"));
    assert!(out.stderr.contains("err") && !out.stderr.contains("out"));
}

#[tokio::test]
async fn cwd_is_workdir() {
    let env = common::Env::new();
    let out = exec(&env, "pwd -P", None).await;
    let expected = std::fs::canonicalize(&env.workdir).unwrap_or(env.workdir.clone());
    assert_eq!(out.stdout.trim(), expected.to_str().unwrap());
}

#[tokio::test]
async fn truncates_above_max_out() {
    // Lower the cap so we don't generate 1 MiB. The handler reads `max_output_bytes`
    // from config at request time, so a custom-configured env takes effect.
    let env = common::Env::with_max_output(32);
    // POSIX-portable: emit exactly 2000 'X' bytes on stdout (no newline).
    let out = exec(&env, "head -c 2000 /dev/zero | tr '\\0' 'X'", None).await;
    assert!(out.stdout.contains("...[truncated:"), "{}", out.stdout);
    assert!(out.stdout.contains("1968 more bytes"), "{}", out.stdout);
    assert!(out.stdout.len() < 2000);
}

#[tokio::test]
async fn at_max_out_is_not_truncated() {
    // Exactly MAX_OUT bytes must NOT be truncated (boundary: len > MAX_OUT).
    let env = common::Env::with_max_output(32);
    let out = exec(&env, "head -c 32 /dev/zero | tr '\\0' 'X'", None).await;
    assert!(!out.stdout.contains("truncated"), "{}", out.stdout);
    assert_eq!(out.stdout.len(), 32);
}

#[tokio::test]
async fn timeout_returns_124() {
    let env = common::Env::new();
    let start = Instant::now();
    let out = exec(&env, "sleep 5", Some(1)).await;
    let elapsed = start.elapsed();
    assert_eq!(out.exit_code, 124);
    assert!(out.timed_out);
    // honoured the short timeout (with slack for scheduling)
    assert!(elapsed.as_secs_f64() < 4.0, "elapsed={elapsed:?}");
}

// --- whole-process-group kill: no orphan survives ---------------------------

const ORPHAN_TOKEN: &str = "2718281828"; // unique sleep duration so pgrep matches only ours

fn count_orphans() -> usize {
    let out = std::process::Command::new("pgrep")
        .args(["-f", &format!("sleep {ORPHAN_TOKEN}")])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count(),
        Err(_) => 0,
    }
}

#[tokio::test]
async fn timeout_kills_whole_process_group() {
    // Background two long sleeps, then block the shell with a third. A timeout
    // must SIGKILL the entire process group, so the backgrounded sleeps must NOT
    // survive as orphans.
    assert_eq!(count_orphans(), 0, "pre-existing orphan before test");

    let env = common::Env::new();
    let cmd = format!("sleep {ORPHAN_TOKEN} & sleep {ORPHAN_TOKEN} & sleep {ORPHAN_TOKEN}");
    let out = exec(&env, &cmd, Some(1)).await;
    assert_eq!(out.exit_code, 124);

    // Give the SIGKILLs a beat to be reaped, then assert no orphan survived.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if count_orphans() == 0 {
            break;
        }
    }
    assert_eq!(count_orphans(), 0, "orphaned child survived the group-kill");
}
