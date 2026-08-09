//! Core of `tests/unit/runtime/test_snapshot_restore.py` — the S3-tiered
//! `/snapshot`+`/restore` surface (#52).
//!
//! The broker is the sole S3 client; the runtime only streams a zstd-compressed
//! tar of the whole workspace off (`GET /snapshot`) and back on (`PUT /restore`)
//! over the per-session key. These drive the round-trip on real tmp workspaces
//! (the native `tar`+`zstd` CLIs are present), the size fail-on-exceed (D9), and
//! the auth gating — 5 cases, matching the Python suite.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};

use common::Bearer;

// --- round-trip -------------------------------------------------------------------

#[tokio::test]
async fn snapshot_restore_roundtrip() {
    // Populate a source workspace.
    let src = common::Env::new();
    std::fs::write(src.workdir.join("hello.txt"), "hello world").unwrap();
    let nested = src.workdir.join("sub").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("data.bin"), [0u8, 1, 2].repeat(100)).unwrap();

    // Snapshot the source workspace.
    let resp = src
        .send(Method::GET, "/snapshot", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/zstd"
    );
    let blob = common::body_bytes(resp).await;
    // zstd magic number — confirms a real zstd stream, not raw tar bytes.
    assert_eq!(&blob[..4], &[0x28, 0xb5, 0x2f, 0xfd], "zstd magic header");

    // Restore into a FRESH, empty workspace (a second isolated runtime).
    let dst = common::Env::new();
    let resp = dst
        .send_bytes(Method::PUT, "/restore", Bearer::Default, blob.to_vec())
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["restored"], true);
    assert_eq!(body["bytes"], blob.len() as u64);

    // Contents round-trip exactly, including the nested binary blob.
    assert_eq!(
        std::fs::read_to_string(dst.workdir.join("hello.txt")).unwrap(),
        "hello world"
    );
    assert_eq!(
        std::fs::read(dst.workdir.join("sub").join("deep").join("data.bin")).unwrap(),
        [0u8, 1, 2].repeat(100)
    );
}

// --- size fail-on-exceed (D9) -----------------------------------------------------

#[tokio::test]
async fn snapshot_refuses_oversized() {
    let env = common::Env::with_max_workspace_bytes(64);
    std::fs::write(env.workdir.join("big.bin"), vec![b'x'; 4096]).unwrap();
    let resp = env
        .send(Method::GET, "/snapshot", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn restore_refuses_oversized() {
    let env = common::Env::with_max_workspace_bytes(64);
    // Garbage (not valid zstd), but the compressed-size cap trips first.
    let resp = env
        .send_bytes(Method::PUT, "/restore", Bearer::Default, vec![b'x'; 4096])
        .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

// --- auth gating (per-session key, #50) ------------------------------------------

#[tokio::test]
async fn snapshot_requires_auth() {
    let env = common::Env::new();
    let resp = env
        .send(Method::GET, "/snapshot", Bearer::None, None, None)
        .await;
    assert!(matches!(
        resp.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
}

#[tokio::test]
async fn restore_requires_auth() {
    let env = common::Env::new();
    let resp = env
        .send_bytes(Method::PUT, "/restore", Bearer::None, Vec::new())
        .await;
    assert!(matches!(
        resp.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
}
