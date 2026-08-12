//! PR-B-5 contract tests: the multipart upload surface (`POST /files/upload`,
//! `POST /upload`) + the zip archive endpoint (`POST /files/archive`).
//!
//! These three handlers were the last open-terminal/LLM-tool endpoints still
//! outstanding; these contract tests prove
//! byte/contract parity.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use std::io::Read;

use common::Bearer;

/// Build a single-file `multipart/form-data` body (field name `file`).
fn multipart(filename: &str, content: &[u8]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "----owui-test-boundary";
    let header = format!(
        "--{BOUNDARY}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    );
    let mut body = Vec::new();
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// Read every regular entry out of an in-memory zip as `(name, content)`.
fn read_zip_entries(bytes: &[u8]) -> Vec<(String, String)> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut za = zip::ZipArchive::new(cursor).expect("valid zip");
    let mut out = Vec::new();
    for i in 0..za.len() {
        let mut f = za.by_index(i).unwrap();
        let name = f
            .enclosed_name().map_or_else(|| f.name().to_string(), |p| p.to_string_lossy().into_owned());
        let mut content = String::new();
        f.read_to_string(&mut content).unwrap();
        out.push((name, content));
    }
    out
}

// --- POST /files/upload -----------------------------------------------------

#[tokio::test]
async fn upload_writes_file_to_base() {
    let env = common::Env::new();
    let (ct, body) = multipart("hello.txt", b"hi there");
    let resp = env
        .send_typed(
            Method::POST,
            "/files/upload",
            Bearer::Default,
            None,
            &ct,
            body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = common::json(resp).await;
    assert_eq!(v["size"], 8);
    let written = std::fs::read_to_string(env.workdir.join("hello.txt")).unwrap();
    assert_eq!(written, "hi there");
}

#[tokio::test]
async fn upload_with_directory_param() {
    let env = common::Env::new();
    std::fs::create_dir_all(env.workdir.join("sub")).unwrap();
    let (ct, body) = multipart("a.txt", b"deep");
    let resp = env
        .send_typed(
            Method::POST,
            "/files/upload?directory=sub",
            Bearer::Default,
            None,
            &ct,
            body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let written = std::fs::read_to_string(env.workdir.join("sub").join("a.txt")).unwrap();
    assert_eq!(written, "deep");
}

#[tokio::test]
async fn upload_creates_missing_directory() {
    let env = common::Env::new();
    let (ct, body) = multipart("a.txt", b"new");
    let resp = env
        .send_typed(
            Method::POST,
            "/files/upload?directory=newdir",
            Bearer::Default,
            None,
            &ct,
            body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let written = std::fs::read_to_string(env.workdir.join("newdir").join("a.txt")).unwrap();
    assert_eq!(written, "new");
}

#[tokio::test]
async fn upload_basename_strips_traversal() {
    let env = common::Env::new();
    // a `filename` carrying a path component is reduced to its basename before
    // join (defense-in-depth), like `os.path.basename`.
    let (ct, body) = multipart("../evil.txt", b"x");
    let resp = env
        .send_typed(
            Method::POST,
            "/files/upload",
            Bearer::Default,
            None,
            &ct,
            body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(env.workdir.join("evil.txt").exists());
    // nothing escaped above the workspace base.
    assert!(!env.workdir.parent().unwrap().join("evil.txt").exists());
}

#[tokio::test]
async fn upload_no_file_field_is_400() {
    let env = common::Env::new();
    // empty multipart (no `file` field)
    let ct = "multipart/form-data; boundary=x";
    let body = b"--x--\r\n".to_vec();
    let resp = env
        .send_typed(
            Method::POST,
            "/files/upload",
            Bearer::Default,
            None,
            ct,
            body,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upload_requires_auth() {
    let env = common::Env::new();
    let (ct, body) = multipart("a.txt", b"x");
    let resp = env
        .send_typed(Method::POST, "/files/upload", Bearer::None, None, &ct, body)
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// --- POST /upload (LLM-tool alias) -----------------------------------------

#[tokio::test]
async fn tool_upload_writes_to_base() {
    let env = common::Env::new();
    let (ct, body) = multipart("tool.bin", b"\x00\x01\x02bytes");
    let resp = env
        .send_typed(Method::POST, "/upload", Bearer::Default, None, &ct, body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = common::json(resp).await;
    assert_eq!(v["bytes"], 8); // \x00\x01\x02 + "bytes" = 3 + 5
    assert!(env.workdir.join("tool.bin").exists());
}

// --- POST /files/archive ----------------------------------------------------

#[tokio::test]
async fn archive_single_file() {
    let env = common::Env::new();
    std::fs::write(env.workdir.join("note.txt"), "hello").unwrap();
    let resp = env
        .send(
            Method::POST,
            "/files/archive",
            Bearer::Default,
            None,
            Some(r#"{"paths":["note.txt"]}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/zip"
    );
    let cd = resp
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cd.contains(r#"filename="note.txt.zip""#), "{cd}");
    let entries = read_zip_entries(&common::body_bytes(resp).await);
    assert_eq!(entries, vec![("note.txt".to_string(), "hello".to_string())]);
}

#[tokio::test]
async fn archive_directory_recurses() {
    let env = common::Env::new();
    std::fs::create_dir_all(env.workdir.join("proj").join("src")).unwrap();
    std::fs::write(env.workdir.join("proj").join("root.md"), "R").unwrap();
    std::fs::write(env.workdir.join("proj").join("src").join("a.rs"), "A").unwrap();
    let resp = env
        .send(
            Method::POST,
            "/files/archive",
            Bearer::Default,
            None,
            Some(r#"{"paths":["proj"]}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut entries = read_zip_entries(&common::body_bytes(resp).await);
    entries.sort();
    assert_eq!(
        entries,
        vec![
            ("proj/root.md".to_string(), "R".to_string()),
            ("proj/src/a.rs".to_string(), "A".to_string()),
        ]
    );
}

#[tokio::test]
async fn archive_missing_path_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/archive",
            Bearer::Default,
            None,
            Some(r#"{"paths":["nope.txt"]}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn archive_empty_paths_is_400() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/archive",
            Bearer::Default,
            None,
            Some(r#"{"paths":[]}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn archive_requires_auth() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/archive",
            Bearer::None,
            None,
            Some(r#"{"paths":["x"]}"#.to_string()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
