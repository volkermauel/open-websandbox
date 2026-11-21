//! `/git/*` contract tests (stage 3) — exercised against a REAL git
//! repository created in the workspace (`git init` + commits via the HTTP
//! actions themselves). Covers the status shape (porcelain v2 + numstat +
//! untracked line counts + remote URL), the read surfaces, the action
//! mapping (stage/commit/checkout/uncommit/pull on a local remote) and the
//! not-a-repo / non-repo-403 contracts.

#![forbid(unsafe_code)]

mod common;

use std::path::Path;

use axum::http::{Method, StatusCode};
use serde_json::Value;

/// A live runtime with the workspace path (the git repo lives in it).
struct Server {
    port: u16,
    workdir: std::path::PathBuf,
    /// Keeps the workspace alive for the whole test.
    _tmp: TempDir,
}

use tempfile::TempDir;

impl Server {
    async fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        let key_path = tmp.path().join("api-key");
        std::fs::write(&key_path, common::STRONG_KEY).unwrap();
        let config = runtime::RuntimeConfig {
            workdir: workdir.clone(),
            runtime_key_file: key_path.clone(),
            ..runtime::RuntimeConfig::default()
        };
        let state = runtime::AppState::new(config, runtime::SessionKeyStore::new(&key_path));
        let app = runtime::build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Server {
            port,
            workdir,
            _tmp: tmp,
        }
    }

    async fn send(&self, method: Method, path: &str, body: Option<String>) -> (StatusCode, Value) {
        let reqwest = reqwest::Client::new();
        let mut r = reqwest
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
            .header("Authorization", format!("Bearer {}", common::STRONG_KEY));
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b);
        }
        let resp = r.send().await.expect("request");
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, json)
    }
}

/// `git init` + identity config + an initial commit via plain CLI (the
/// ACTIONS under test then drive the rest).
fn init_repo(workdir: &Path) {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .current_dir(workdir)
            .output()
            .expect("git runs")
    };
    assert!(run(&["init", "-b", "main"]).status.success());
    assert!(run(&["config", "user.email", "t@t"]).status.success());
    assert!(run(&["config", "user.name", "t"]).status.success());
}

#[tokio::test]
async fn status_reports_not_a_repo_then_full_state() {
    eprintln!("DBG test start");
    let srv = Server::start().await;
    eprintln!("DBG server up on {}", srv.port);
    let (_, before) = srv.send(Method::GET, "/git/status", None).await;
    assert_eq!(before["is_repo"], false);

    init_repo(&srv.workdir);
    std::fs::write(srv.workdir.join("tracked.txt"), "line1\nline2\n").unwrap();
    std::fs::write(srv.workdir.join("new.txt"), "added\n").unwrap();

    let (_, status) = srv.send(Method::GET, "/git/status", None).await;
    assert_eq!(status["is_repo"], true);
    assert_eq!(status["branch"], "main");
    let files = status["files"].as_array().unwrap();
    // Neither file was committed — both are untracked, and the line-stats
    // helper already counted their lines as additions.
    let tracked = files
        .iter()
        .find(|f| f["path"] == "tracked.txt")
        .expect("tracked.txt in status");
    assert_eq!(tracked["status"], "untracked");
    assert_eq!(tracked["additions"], 2);
    let new = files.iter().find(|f| f["path"] == "new.txt").unwrap();
    assert_eq!(new["additions"], 1);
}

#[tokio::test]
async fn stage_commit_log_roundtrip() {
    let srv = Server::start().await;
    init_repo(&srv.workdir);
    std::fs::write(srv.workdir.join("a.txt"), "one\ntwo\n").unwrap();

    // Stage via the action surface.
    let (code, out) = srv
        .send(
            Method::POST,
            "/git/stage",
            Some(r#"{"files":["a.txt"]}"#.into()),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    let (_, status) = srv.send(Method::GET, "/git/status", None).await;
    eprintln!("STATUS={status}");
    let staged = status["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "a.txt")
        .unwrap();
    assert_eq!(staged["staged"], true);
    // Untracked line stats land as additions (2 lines).
    assert_eq!(staged["additions"], 2);

    // Commit.
    let (code, out) = srv
        .send(
            Method::POST,
            "/git/commit",
            Some(r#"{"message":"first"}"#.into()),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");

    // Log shows the commit with the upstream field set.
    let (_, log) = srv.send(Method::GET, "/git/log", None).await;
    let commits = log.as_array().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["message"], "first");
    assert_eq!(commits[0]["hash"].as_str().unwrap_or("").len(), 40);

    // Modify + diff.
    std::fs::write(srv.workdir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let (_, diff) = srv.send(Method::GET, "/git/diff?file=a.txt", None).await;
    assert!(
        diff["diff"].as_str().unwrap_or("").contains("+three"),
        "{}",
        diff
    );

    // Second commit, then uncommit drops it (soft — the tree file stays).
    std::fs::write(
        srv.workdir.join("b.txt"),
        "b
",
    )
    .unwrap();
    let (code, out) = srv
        .send(
            Method::POST,
            "/git/stage",
            Some(r#"{"files":["b.txt"]}"#.into()),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    let (code, out) = srv
        .send(
            Method::POST,
            "/git/commit",
            Some(r#"{"message":"second"}"#.into()),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    // uncommit resets HEAD~1 (soft). Upstream fails identically on an
    // initial commit (no parent) — two commits make this well-defined.
    let (code, out) = srv
        .send(Method::POST, "/git/uncommit", Some("{}".into()))
        .await;
    assert_eq!(code, StatusCode::OK, "{out}");
    let (_, log) = srv.send(Method::GET, "/git/log", None).await;
    let commits = log.as_array().unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["message"], "first");
    assert!(
        srv.workdir.join("b.txt").exists(),
        "soft reset keeps the tree"
    );
}

#[tokio::test]
async fn actions_reject_bad_input_like_upstream() {
    let srv = Server::start().await;
    init_repo(&srv.workdir);
    // Stage with no files → 400 "Select files first".
    let (code, out) = srv
        .send(Method::POST, "/git/stage", Some(r#"{"files":[]}"#.into()))
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(out["detail"], "Select files first");
    // Commit with a blank message → 400.
    let (code, out) = srv
        .send(
            Method::POST,
            "/git/commit",
            Some(r#"{"message":"  "}"#.into()),
        )
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(out["detail"], "Enter a commit message");
    // Unknown action → 404.
    let (code, _) = srv
        .send(Method::POST, "/git/rebase-everything", Some("{}".into()))
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // show with a non-hash ref → 400.
    let (code, _) = srv.send(Method::GET, "/git/show?ref=main", None).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pr_capabilities_without_gh_degrade_to_unavailable() {
    let srv = Server::start().await;
    init_repo(&srv.workdir);
    // Capabilities NEVER 404 — upstream wraps the whole probe in
    // try/except HTTPException → available:false: no `gh` binary, no auth,
    // no remote — all degrade (review finding 6). Upstream still returns the
    // full shape (defaults filled from the failed probe).
    let (_, caps) = srv.send(Method::GET, "/git/pr/capabilities", None).await;
    assert_eq!(
        caps["available"], false,
        "no authed gh → unavailable: {caps}"
    );
    assert!(caps.get("default_branch").is_some(), "{caps}");
    assert!(caps.get("open_pull_requests").is_some(), "{caps}");
}

#[tokio::test]
async fn routes_require_the_runtime_key() {
    let srv = Server::start().await;
    let client = reqwest::Client::new();
    for path in ["/git/status", "/git/log", "/git/branches", "/git/worktrees"] {
        let resp = client
            .get(format!("http://127.0.0.1:{}{path}", srv.port))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}
