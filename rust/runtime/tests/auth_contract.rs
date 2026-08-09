//! Verbatim port of `tests/unit/runtime/test_runtime_auth.py` (9 cases).
//!
//! The Python suite drives the guard through `POST /api/terminals`, which lands
//! in PR-B-3 (PTY terminal). Here we exercise the identical guard through
//! `POST /execute` instead — the auth contract is the same per-session key
//! check; only the gated vehicle differs.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use runtime::SessionKeyStore;

use common::Bearer;

/// Execute JSON body for a fast, harmless command.
const EXEC_TRUE: &str = r#"{"command":"true","timeout":5}"#;

// --- startup guard -----------------------------------------------------------

#[test]
fn validate_rejects_weak_key() {
    for bad in [
        "",
        "dev-shared-secret-change-me",
        "change-me",
        "changeme",
        "placeholder",
    ] {
        let dir = tempfile::TempDir::new().unwrap();
        let key_path = dir.path().join("api-key");
        std::fs::write(&key_path, bad).unwrap();
        let store = SessionKeyStore::new(&key_path);
        assert!(
            store.validate().is_err(),
            "{bad:?} should be rejected at boot"
        );
    }
}

#[test]
fn validate_rejects_missing_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = SessionKeyStore::new(dir.path().join("does-not-exist"));
    assert!(store.validate().is_err());
}

#[test]
fn validate_accepts_strong_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let key_path = dir.path().join("api-key");
    std::fs::write(&key_path, "a-very-strong-and-random-runtime-key-123456").unwrap();
    let store = SessionKeyStore::new(&key_path);
    store.validate().expect("strong key should boot");
}

// --- request guard on the gated surface (via /execute) -----------------------

#[tokio::test]
async fn rejects_missing_bearer() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::None,
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::UNAUTHORIZED).await;
}

#[tokio::test]
async fn rejects_wrong_bearer() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Explicit("nope"),
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::UNAUTHORIZED).await;
}

#[tokio::test]
async fn accepts_correct_bearer() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Default,
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::OK).await;
}

#[tokio::test]
async fn denies_503_when_key_unset() {
    // Deny-on-unset (defense-in-depth): with the key file missing the request
    // guard 503s at the request path, independent of the startup boot guard.
    let env = common::Env::new();
    std::fs::remove_file(&env.key_path).unwrap();
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Default,
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::SERVICE_UNAVAILABLE).await;
}

#[tokio::test]
async fn reloads_on_rotate() {
    // Rotate-on-resume: a cached key just rotated (fresh Secret synced -> new
    // mtime) must be honored WITHOUT a restart. Seed the cache with old-key,
    // rotate, and assert the NEW key is accepted (and old rejected) next request.
    let env = common::Env::new();
    env.set_key("old-key");
    // populate the cache with the old value
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Explicit("old-key"),
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::OK).await;

    let new_key = env.rotate_key();

    // the old key must now be rejected ...
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Explicit("old-key"),
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::UNAUTHORIZED).await;

    // ... and the freshly rotated key accepted on the next request.
    let resp = env
        .send(
            Method::POST,
            "/execute",
            Bearer::Explicit(&new_key),
            None,
            Some(EXEC_TRUE.into()),
        )
        .await;
    common::assert_status(resp, StatusCode::OK).await;
}

// --- route-table-driven auth invariant (regression guard) --------------------
/// Every app-defined route except the open health/info endpoints (GET /,
/// GET /healthz, GET /readyz) 401s without a Bearer. Any newly-added ungated
/// route fails this test on purpose — the Python `test_full_surface_auth_invariant`.
#[tokio::test]
async fn full_surface_auth_invariant() {
    let env = common::Env::new();
    // (method, path, body) for the FULL gated surface. A 401 short-circuits
    // before any body validation, so placeholder bodies are fine.
    let gated: &[(Method, &str, Option<String>)] = &[
        (Method::POST, "/execute", Some(EXEC_TRUE.into())),
        (Method::GET, "/files/cwd", None),
        (Method::POST, "/files/cwd", Some(r#"{"path":"."}"#.into())),
        (Method::GET, "/files/list?directory=.", None),
        (Method::GET, "/files/read?path=x", None),
        (
            Method::POST,
            "/files/write",
            Some(r#"{"path":"x","content":""}"#.into()),
        ),
        (Method::POST, "/files/mkdir", Some(r#"{"path":"x"}"#.into())),
        (
            Method::POST,
            "/files/move",
            Some(r#"{"source":"a","destination":"b"}"#.into()),
        ),
        (Method::DELETE, "/files/delete?path=x", None),
    ];
    for (method, path, body) in gated {
        let resp = env
            .send(method.clone(), path, Bearer::None, None, body.clone())
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{} {path}: expected 401, got {}",
            method,
            resp.status()
        );
    }
    // The three open routes stay 200 without a Bearer.
    for path in ["/", "/healthz", "/readyz"] {
        let resp = env.send(Method::GET, path, Bearer::None, None, None).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET {path}: expected 200, got {}",
            resp.status()
        );
    }
}
