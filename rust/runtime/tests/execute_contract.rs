//! Execute-contract tests (the stage-3 background-process model).
//!
//! Real `tokio` spawns through the HTTP surface: the upstream log-shape
//! response (`{id, command, cwd, status, exit_code, output[], truncated,
//! next_offset}`), `?wait=` inline completion, the running→done lifecycle,
//! stdin input, group kill with no orphans, list/status offsets and the
//! runtime-extension `timeout` body field. `/bin/sh` (dash) is the shell, so
//! every command is POSIX-portable (no bash-isms).

#![forbid(unsafe_code)]

mod common;

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use serde_json::Value;

use common::Bearer;

/// POST /execute with an optional body override (`timeout` extension) and
/// `?wait=` — asserts HTTP 200 and returns the snapshot.
async fn exec(env: &common::Env, command: &str, wait: Option<u64>, timeout: Option<u64>) -> Value {
    let mut body = serde_json::json!({ "command": command });
    if let Some(t) = timeout {
        body["timeout"] = serde_json::json!(t);
    }
    let path = match wait {
        Some(w) => format!("/execute?wait={w}"),
        None => "/execute".to_string(),
    };
    let resp = env
        .send(
            Method::POST,
            &path,
            Bearer::Default,
            None,
            Some(body.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "execute should always 200");
    common::json(resp).await
}

/// Join the `stdout` records of a snapshot into one string (the helper the
/// rest of the assertions read).
fn stdout_of(snapshot: &Value) -> String {
    snapshot["output"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .filter(|r| r["type"] == "stdout")
                .filter_map(|r| r["data"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

fn stderr_of(snapshot: &Value) -> String {
    snapshot["output"]
        .as_array()
        .map(|records| {
            records
                .iter()
                .filter(|r| r["type"] == "stderr")
                .filter_map(|r| r["data"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Poll `GET /execute/{id}/status` until the process leaves `running`.
async fn await_done(env: &common::Env, id: &str) -> Value {
    for _ in 0..100 {
        let resp = env
            .send(
                Method::GET,
                &format!("/execute/{id}/status"),
                Bearer::Default,
                None,
                None,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "status poll should 200");
        let snapshot: Value = common::json(resp).await;
        if snapshot["status"] != "running" {
            return snapshot;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process {id} never finished");
}

#[tokio::test]
async fn exit_code_nonzero() {
    let env = common::Env::new();
    let out = exec(&env, "exit 7", Some(5), None).await;
    assert_eq!(out["status"], "done");
    assert_eq!(out["exit_code"], 7);
}

#[tokio::test]
async fn exit_code_zero() {
    let env = common::Env::new();
    let out = exec(&env, "true", Some(5), None).await;
    assert_eq!(out["exit_code"], 0);
}

#[tokio::test]
async fn stdout_capture_with_trailing_newline() {
    let env = common::Env::new();
    let out = exec(&env, "echo hello", Some(5), None).await;
    // asyncio readline parity: the trailing newline is RETAINED in `data`.
    let records = out["output"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "stdout");
    assert_eq!(records[0]["data"], "hello\n");
    assert_eq!(out["next_offset"], 1);
    assert_eq!(out["truncated"], false);
}

#[tokio::test]
async fn stderr_split() {
    let env = common::Env::new();
    let out = exec(&env, "echo out; echo err 1>&2", Some(5), None).await;
    let stdout = stdout_of(&out);
    let stderr = stderr_of(&out);
    assert!(stdout.contains("out") && !stdout.contains("err"));
    assert!(stderr.contains("err") && !stderr.contains("out"));
}

#[tokio::test]
async fn cwd_is_workdir() {
    let env = common::Env::new();
    let out = exec(&env, "pwd -P", Some(5), None).await;
    let expected = std::fs::canonicalize(&env.workdir).unwrap_or(env.workdir.clone());
    // The response carries the resolved cwd (0.17.x `#184` field)…
    assert_eq!(
        out["cwd"].as_str().unwrap(),
        expected.to_str().unwrap(),
        "response.cwd is the workspace base"
    );
    // …and the command actually ran there.
    assert_eq!(stdout_of(&out).trim(), expected.to_str().unwrap());
}

#[tokio::test]
async fn cwd_param_resolves_inside_workspace() {
    let env = common::Env::new();
    let subdir = env.workdir.join("sub-cwd");
    std::fs::create_dir_all(&subdir).unwrap();
    let resp = env
        .send(
            Method::POST,
            "/execute?wait=5",
            Bearer::Default,
            None,
            Some(serde_json::json!({"command": "pwd -P", "cwd": "sub-cwd"}).to_string()),
        )
        .await;
    let out = common::json(resp).await;
    assert_eq!(stdout_of(&out).trim(), subdir.to_str().unwrap());
    // An escaping cwd is a 400 (confinement contract).
    let resp = env
        .send(
            Method::POST,
            "/execute?wait=1",
            Bearer::Default,
            None,
            Some(serde_json::json!({"command": "true", "cwd": "../.."}).to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn background_running_then_done_with_offset_reads() {
    let env = common::Env::new();
    let started = exec(&env, "echo one; echo two; echo three", None, None).await;
    let id = started["id"]
        .as_str()
        .expect("process id in snapshot")
        .to_owned();
    assert_eq!(started["status"], "running");
    let done = await_done(&env, &id).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["exit_code"], 0);
    assert_eq!(done["output"].as_array().unwrap().len(), 3);
    // Incremental read from offset 1 → lines two+three, same next_offset.
    let resp = env
        .send(
            Method::GET,
            &format!("/execute/{id}/status?offset=1"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    let window: Value = common::json(resp).await;
    assert_eq!(window["output"].as_array().unwrap().len(), 2);
    assert_eq!(window["output"][0]["data"], "two\n");
    assert_eq!(window["next_offset"], 3);
    assert_eq!(window["truncated"], false);
    // tail=2 over the full window truncates the head.
    let resp = env
        .send(
            Method::GET,
            &format!("/execute/{id}/status?tail=2"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    let tailed: Value = common::json(resp).await;
    assert_eq!(tailed["output"].as_array().unwrap().len(), 2);
    assert_eq!(tailed["output"][0]["data"], "two\n");
    assert_eq!(tailed["truncated"], true);
    // The dock list carries the finished entry with its status.
    let resp = env
        .send(Method::GET, "/execute", Bearer::Default, None, None)
        .await;
    let list: Value = common::json(resp).await;
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id.as_str())
        .expect("finished entry still listed");
    assert_eq!(entry["status"], "done");
    assert_eq!(entry["exit_code"], 0);
}

#[tokio::test]
async fn input_and_kill_lifecycle() {
    let env = common::Env::new();
    let started = exec(&env, "cat", None, None).await;
    let id = started["id"].as_str().unwrap().to_owned();
    assert_eq!(started["status"], "running");
    assert_eq!(started["exit_code"], serde_json::Value::Null);

    // Literal escape sequences are decoded like upstream's unicode_escape.
    let resp = env
        .send(
            Method::POST,
            &format!("/execute/{id}/input"),
            Bearer::Default,
            None,
            Some(serde_json::json!({"input": r"ping\n"}).to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let resp = env
            .send(
                Method::GET,
                &format!("/execute/{id}/status"),
                Bearer::Default,
                None,
                None,
            )
            .await;
        let snapshot: Value = common::json(resp).await;
        if stdout_of(&snapshot).contains("ping\n") {
            break;
        }
        assert!(Instant::now() < deadline, "echoed input never surfaced");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Kill (SIGKILL — cat has no TERM handler guarantee).
    let resp = env
        .send(
            Method::DELETE,
            &format!("/execute/{id}?force=true"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let killed = await_done(&env, &id).await;
    assert_eq!(killed["status"], "killed");
    // Unknown ids 404.
    let resp = env
        .send(
            Method::GET,
            "/execute/does-not-exist/status",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn timeout_extension_kills_and_reports_killed() {
    let env = common::Env::new();
    let start = Instant::now();
    // Runtime extension: `timeout` SIGKILLs at the deadline (`status=killed`,
    // negative signal number — Python returncode parity).
    let out = exec(&env, "sleep 5", Some(5), Some(1)).await;
    let elapsed = start.elapsed();
    assert_eq!(out["status"], "killed");
    assert_eq!(out["exit_code"], -9);
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
    // Background two long sleeps, then block the shell with a third. The
    // extension deadline must SIGKILL the entire process group, so the
    // backgrounded sleeps must NOT survive as orphans.
    assert_eq!(count_orphans(), 0, "pre-existing orphan before test");

    let env = common::Env::new();
    let cmd = format!("sleep {ORPHAN_TOKEN} & sleep {ORPHAN_TOKEN} & sleep {ORPHAN_TOKEN}");
    let out = exec(&env, &cmd, Some(5), Some(1)).await;
    assert_eq!(out["status"], "killed");

    // Give the SIGKILLs a beat to be reaped, then assert no orphan survived.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if count_orphans() == 0 {
            break;
        }
    }
    assert_eq!(count_orphans(), 0, "orphaned child survived the group-kill");
}
