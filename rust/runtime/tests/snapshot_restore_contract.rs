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

use std::fs;
use std::process::Command;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Method, StatusCode};
use common::Bearer;
use tempfile::TempDir;

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

// --- cross-interop with standard `tar`/`zstd` CLIs (issue #94 acc.#6) ----------

/// `tar --help` advertises `--zstd`? Skips (does not fail) the interop tests on
/// toolchains without it, so `cargo test --all` stays green everywhere.
fn tar_supports_zstd() -> bool {
    let Ok(out) = Command::new("tar").arg("--help").output() else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains("--zstd")
}

// (a) archive produced by the new native code -> extractable by `tar --zstd`.
#[tokio::test]
async fn snapshot_output_extracts_with_gnu_tar_zstd() {
    if !tar_supports_zstd() {
        eprintln!("skipping: system tar lacks --zstd");
        return;
    }
    let env = common::Env::new();
    std::fs::write(env.workdir.join("top.txt"), b"top").unwrap();
    std::fs::create_dir_all(env.workdir.join("d/e")).unwrap();
    std::fs::write(env.workdir.join("d/e/f.bin"), vec![7u8; 1234]).unwrap();

    let resp = env
        .send(Method::GET, "/snapshot", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let blob = common::body_bytes(resp).await;
    assert_eq!(&blob[..4], &[0x28, 0xb5, 0x2f, 0xfd], "zstd magic header");

    // Write our zstd stream to disk and hand it to GNU tar.
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("snap.tar.zst");
    fs::write(&archive, &blob).unwrap();
    let outdir = tmp.path().join("out");
    fs::create_dir_all(&outdir).unwrap();
    let status = Command::new("tar")
        .arg("--zstd")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&outdir)
        .status()
        .expect("run tar");
    assert!(
        status.success(),
        "tar --zstd failed to extract our output: {status}"
    );

    // Logical content equality (Q3: byte-identical NOT required).
    assert_eq!(fs::read_to_string(outdir.join("top.txt")).unwrap(), "top");
    assert_eq!(fs::read(outdir.join("d/e/f.bin")).unwrap(), vec![7u8; 1234]);
}

// (b) archive produced by `tar --zstd` (CLI) on a fixture dir -> restorable by us.
#[tokio::test]
async fn cli_tar_zstd_archive_restores_into_workspace() {
    if !tar_supports_zstd() {
        eprintln!("skipping: system tar lacks --zstd");
        return;
    }
    // A fixture dir OUTSIDE any workspace, with nested/binary/symlink content.
    let fix = TempDir::new().unwrap();
    fs::write(fix.path().join("a.txt"), b"alpha").unwrap();
    fs::create_dir_all(fix.path().join("sub")).unwrap();
    fs::write(
        fix.path().join("sub").join("b.bin"),
        (0u8..50).collect::<Vec<u8>>(),
    )
    .unwrap();
    std::os::unix::fs::symlink("a.txt", fix.path().join("link.txt")).unwrap();

    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("cli.tar.zst");
    let status = Command::new("tar")
        .arg("--zstd")
        .arg("-cf")
        .arg(&archive)
        .arg("-C")
        .arg(fix.path())
        .arg(".")
        .status()
        .expect("run tar");
    assert!(status.success(), "tar --zstd create failed: {status}");
    let blob = fs::read(&archive).unwrap();

    // Restore the CLI-produced archive into a FRESH workspace.
    let env = common::Env::new();
    let resp = env
        .send_bytes(Method::PUT, "/restore", Bearer::Default, blob)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["restored"], true);

    assert_eq!(
        fs::read_to_string(env.workdir.join("a.txt")).unwrap(),
        "alpha"
    );
    assert_eq!(
        fs::read(env.workdir.join("sub").join("b.bin")).unwrap(),
        (0u8..50).collect::<Vec<u8>>()
    );
    assert!(env.workdir.join("link.txt").is_symlink());
    assert_eq!(
        std::fs::read_link(env.workdir.join("link.txt"))
            .unwrap()
            .to_string_lossy(),
        "a.txt"
    );
}

// --- client disconnect mid-stream ----------------------------------------

// Restore: a client that disconnects mid-upload (stream errors partway) must NOT
// get a 200 — the handler surfaces the body-read failure as a 500.
#[tokio::test]
async fn restore_client_disconnect_midstream_is_error_not_ok() {
    let env = common::Env::new();
    std::fs::write(env.workdir.join("x.txt"), b"xxxxxxxxxx").unwrap();

    // Produce a real snapshot, then stream only its first half before erroring.
    let resp = env
        .send(Method::GET, "/snapshot", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let blob = common::body_bytes(resp).await;
    let half = blob[..blob.len() / 2].to_vec();
    let stream = futures_util::stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from(half)),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "client disconnected mid-stream",
        )),
    ]);
    let resp = env
        .send_body(
            Method::PUT,
            "/restore",
            Bearer::Default,
            Body::from_stream(stream),
        )
        .await;
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "mid-stream disconnect must not return 200"
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// Snapshot: a client that opens the stream and immediately disconnects (body
// dropped unread) must reap the in-flight producer — proven by a follow-up
// snapshot still working (no deadlock / leaked blocking thread).
#[tokio::test]
async fn snapshot_client_disconnect_reaps_producer() {
    let env = common::Env::new();
    std::fs::write(env.workdir.join("a.txt"), b"aaaa").unwrap();

    let resp = tokio::time::timeout(
        Duration::from_secs(10),
        env.send(Method::GET, "/snapshot", Bearer::Default, None, None),
    )
    .await
    .expect("snapshot GET hung");
    assert_eq!(resp.status(), StatusCode::OK);
    // Simulate the client vanishing mid-stream: drop the unread body.
    drop(resp);

    // The dropped response body errors the encoder's channel writes, reaping the
    // spawn_blocking tar producer — a fresh snapshot must succeed immediately.
    let resp2 = env
        .send(Method::GET, "/snapshot", Bearer::Default, None, None)
        .await;
    assert_eq!(resp2.status(), StatusCode::OK);
}
