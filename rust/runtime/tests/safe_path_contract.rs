//! Verbatim port of `tests/unit/runtime/test_safe_path.py` (17 cases).
//!
//! The path-confinement boundary is the #1 security deliverable of PR-B-1; these
//! cases throw the usual traversal arsenal at [`safe_path`] directly (function
//! level) and again through the HTTP endpoints (integration level).

#![forbid(unsafe_code)]

mod common;

use std::path::{Path, PathBuf};

use axum::http::Method;
use runtime::safe_path::{request_base, safe_path};

use common::{Bearer, Env};

// --- helpers -----------------------------------------------------------------

fn assert_escape(rel: &str, base: &Path) {
    let err = safe_path(rel, base)
        .err()
        .unwrap_or_else(|| panic!("{rel:?} should be rejected"));
    let status = axum::response::IntoResponse::into_response(err).status();
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "{rel:?} should be rejected (got {status})"
    );
}

fn assert_within(rel: &str, base: &Path) -> PathBuf {
    let full = safe_path(rel, base).unwrap_or_else(|_| panic!("{rel:?} should be allowed"));
    assert!(
        full == base || full.starts_with(base),
        "{rel:?} escaped base: {full:?}"
    );
    full
}

// --- direct function tests ---------------------------------------------------

#[test]
fn rejects_dotdot_traversal() {
    let wd = tmp_workdir();
    assert_escape("../../etc/passwd", &wd);
    assert_escape("../../../etc/passwd", &wd);
}

#[test]
fn rejects_absolute_outside() {
    let wd = tmp_workdir();
    assert_escape("/etc/passwd", &wd);
    assert_escape("/etc", &wd);
    assert_escape("/root/.ssh/id_rsa", &wd);
}

#[test]
fn rejects_url_encoded_traversal() {
    let wd = tmp_workdir();
    // %2e%2e == "..", %2f == "/" — must be decoded first, then confined.
    assert_escape("%2e%2e/%2e%2e/etc/passwd", &wd);
    assert_escape("%2e%2e%2f%2e%2e%2fetc%2fpasswd", &wd);
}

#[test]
fn rejects_url_encoded_absolute() {
    let wd = tmp_workdir();
    assert_escape("%2fetc%2fpasswd", &wd);
}

#[test]
fn rejects_symlink_escape() {
    let wd = tmp_workdir();
    // A symlink living *inside* base pointing *outside* must not leak.
    std::os::unix::fs::symlink("/etc", wd.join("etc_link")).unwrap();
    assert_escape("etc_link", &wd);
    assert_escape("etc_link/passwd", &wd);
    // ... but a symlink to somewhere still inside base is fine.
    let inner_target = wd.join("real");
    std::fs::create_dir_all(&inner_target).unwrap();
    std::os::unix::fs::symlink(&inner_target, wd.join("good_link")).unwrap();
    assert_within("good_link", &wd);
}

#[test]
fn accepts_legitimate_relative() {
    let wd = tmp_workdir();
    assert_within("foo.txt", &wd);
    assert_within("a/b/c.txt", &wd);
    // internal ".." normalisation that stays inside base is allowed.
    let via_dotdot = assert_within("a/../bar", &wd);
    assert_eq!(via_dotdot, wd.join("bar"));
}

#[test]
fn accepts_base_itself() {
    let wd = tmp_workdir();
    assert_eq!(safe_path(".", &wd).unwrap(), wd);
    assert_eq!(safe_path("", &wd).unwrap(), wd);
}

#[test]
fn absolute_inside_base_honoured() {
    let wd = tmp_workdir();
    let inside = wd.join("nested").join("f.txt");
    assert_eq!(safe_path(inside.to_str().unwrap(), &wd).unwrap(), inside);
}

#[test]
fn windows_separators_stay_confined() {
    let wd = tmp_workdir();
    // Backslashes are NOT separators on Linux, so this is a single literal
    // filename component under base — it must NOT escape (stays within base).
    let res = safe_path("..\\..\\etc\\passwd", &wd).unwrap();
    assert!(
        res == wd || res.starts_with(&wd),
        "backslash vector escaped: {res:?}"
    );
}

// --- _request_base / X-Workspace-Subdir confinement (HTTP integration) -------

#[tokio::test]
async fn subdir_rejects_slashes() {
    let env = Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/cwd",
            Bearer::Default,
            Some("a/b"),
            None,
        )
        .await;
    common::assert_status(resp, axum::http::StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn subdir_rejects_traversal() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/files/cwd", Bearer::Default, Some(".."), None)
        .await;
    common::assert_status(resp, axum::http::StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn subdir_rejects_too_long() {
    let env = Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/cwd",
            Bearer::Default,
            Some(&"x".repeat(65)),
            None,
        )
        .await;
    common::assert_status(resp, axum::http::StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn subdir_creates_and_confines() {
    let env = Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/cwd",
            Bearer::Default,
            Some("chat1"),
            None,
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let cwd = body["cwd"].as_str().unwrap();
    assert!(cwd.ends_with("/chat1"), "{cwd}");
    assert!(
        cwd.starts_with(&format!("{}/", env.workdir.display())),
        "{cwd}"
    );

    // write a secret in chat1
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            Some("chat1"),
            Some(r#"{"path":"secret.txt","content":"topsecret"}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // a *different* subdir cannot traverse into chat1
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=../chat1/secret.txt",
            Bearer::Default,
            Some("chat2"),
            None,
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

    // chat1 itself cannot traverse above WORKDIR either
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=../../etc/passwd",
            Bearer::Default,
            Some("chat1"),
            None,
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

// --- HTTP-level traversal against the endpoints ------------------------------

#[tokio::test]
async fn http_read_rejects_traversal() {
    let env = Env::new();
    for path in [
        "../../etc/passwd",
        "/etc/passwd",
        "%2e%2e/%2e%2e/etc/passwd",
    ] {
        let resp = env
            .send(
                Method::GET,
                &format!("/files/read?path={path}"),
                Bearer::Default,
                None,
                None,
            )
            .await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "path={path}"
        );
    }
}

#[tokio::test]
async fn http_write_rejects_traversal() {
    let env = Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            None,
            Some(r#"{"path":"../../tmp/evil","content":"x"}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_list_rejects_traversal() {
    let env = Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/list?directory=../../etc",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn http_delete_rejects_traversal() {
    let env = Env::new();
    let resp = env
        .send(
            Method::DELETE,
            "/files/delete?path=../../etc/passwd",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
}

// --- fixtures ----------------------------------------------------------------

fn tmp_workdir() -> PathBuf {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    // Canonicalise so comparisons match `os.path.realpath`.
    let canon = std::fs::canonicalize(&path).unwrap_or(path);
    // Leak: the dir outlives the test process; fine for a unit test.
    std::mem::forget(dir);
    canon
}

/// Smoke: `request_base` + `safe_path` compose under [`AppState`] (guards
/// against the helper drifting from production wiring).
#[test]
fn request_base_smoke() {
    let wd = tmp_workdir();
    let base = request_base(&wd, None).unwrap();
    assert_eq!(base, wd);
}
