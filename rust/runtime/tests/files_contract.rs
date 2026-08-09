//! Core of `tests/unit/runtime/test_files_api.py` — the open-terminal `/files/*`
//! round-trip surface in scope for PR-B-1 (the grep/glob/upload/archive/view and
//! LLM-tool handlers arrive in PR-B-4).
//!
//! Covers: health, get/set cwd, write→read→list round-trip, mkdir/move/delete,
//! and the 404/409 error contracts.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};

use common::Bearer;

// --- misc endpoints ----------------------------------------------------------

#[tokio::test]
async fn health() {
    let env = common::Env::new();
    let resp = env.send(Method::GET, "/", Bearer::None, None, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(
        body,
        serde_json::json!({"status":"ok","runtime":"code-standard"})
    );
}

#[tokio::test]
async fn healthz_and_readyz() {
    let env = common::Env::new();
    for path in ["/healthz", "/readyz"] {
        let resp = env.send(Method::GET, path, Bearer::None, None, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn get_cwd_reports_workdir() {
    let env = common::Env::new();
    let resp = env
        .send(Method::GET, "/files/cwd", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["cwd"], env.workdir.to_str().unwrap());
    assert_eq!(body["home"], env.workdir.to_str().unwrap());
}

#[tokio::test]
async fn set_cwd_requires_existing_dir() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/cwd",
            Bearer::Default,
            None,
            Some(r#"{"path":"nope"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // create then set
    env.send(
        Method::POST,
        "/files/mkdir",
        Bearer::Default,
        None,
        Some(r#"{"path":"realdir"}"#.into()),
    )
    .await;
    let resp = env
        .send(
            Method::POST,
            "/files/cwd",
            Bearer::Default,
            None,
            Some(r#"{"path":"realdir"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert!(body["cwd"].as_str().unwrap().ends_with("realdir"));
}

// --- core write/read/list round-trip -----------------------------------------

#[tokio::test]
async fn write_read_round_trip() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            None,
            Some(r#"{"path":"dir/a.txt","content":"hello"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["size"], 5);
    assert!(body["path"].as_str().unwrap().ends_with("dir/a.txt"));

    let resp = env
        .send(
            Method::GET,
            "/files/read?path=dir/a.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let data: serde_json::Value = common::json(resp).await;
    assert_eq!(data["content"], "hello");
    assert_eq!(data["total_lines"], 1);
}

#[tokio::test]
async fn read_missing_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=nope.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_entries() {
    let env = common::Env::new();
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"a.txt","content":"aaa"}"#.into()),
    )
    .await;
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"b.txt","content":"bbbbb"}"#.into()),
    )
    .await;
    env.send(
        Method::POST,
        "/files/mkdir",
        Bearer::Default,
        None,
        Some(r#"{"path":"sub"}"#.into()),
    )
    .await;

    let resp = env
        .send(
            Method::GET,
            "/files/list?directory=.",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let names: std::collections::HashSet<String> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    for must in ["a.txt", "b.txt", "sub"] {
        assert!(names.contains(must), "missing {must}: {names:?}");
    }
    let types: std::collections::HashMap<&str, &str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap(), e["type"].as_str().unwrap()))
        .collect();
    assert_eq!(types.get("sub"), Some(&"directory"));
    assert_eq!(types.get("a.txt"), Some(&"file"));
}

#[tokio::test]
async fn list_missing_dir_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/list?directory=ghost",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- mkdir / move / delete ---------------------------------------------------

#[tokio::test]
async fn move_entry() {
    let env = common::Env::new();
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"src.txt","content":"data"}"#.into()),
    )
    .await;
    let resp = env
        .send(
            Method::POST,
            "/files/move",
            Bearer::Default,
            None,
            Some(r#"{"source":"src.txt","destination":"dst.txt"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // source gone, destination has the content
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=src.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=dst.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["content"], "data");
}

#[tokio::test]
async fn move_collision_is_409() {
    let env = common::Env::new();
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"a","content":"1"}"#.into()),
    )
    .await;
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"b","content":"2"}"#.into()),
    )
    .await;
    let resp = env
        .send(
            Method::POST,
            "/files/move",
            Bearer::Default,
            None,
            Some(r#"{"source":"a","destination":"b"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn move_missing_source_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::POST,
            "/files/move",
            Bearer::Default,
            None,
            Some(r#"{"source":"nope","destination":"x"}"#.into()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_file_and_dir() {
    let env = common::Env::new();
    env.send(
        Method::POST,
        "/files/write",
        Bearer::Default,
        None,
        Some(r#"{"path":"del.txt","content":"x"}"#.into()),
    )
    .await;
    env.send(
        Method::POST,
        "/files/mkdir",
        Bearer::Default,
        None,
        Some(r#"{"path":"deldir/sub"}"#.into()),
    )
    .await;

    let resp = env
        .send(
            Method::DELETE,
            "/files/delete?path=del.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["type"], "file");

    let resp = env
        .send(
            Method::DELETE,
            "/files/delete?path=deldir",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["type"], "directory");

    // gone: read returns 404
    let resp = env
        .send(
            Method::GET,
            "/files/read?path=del.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_missing_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::DELETE,
            "/files/delete?path=ghost",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
