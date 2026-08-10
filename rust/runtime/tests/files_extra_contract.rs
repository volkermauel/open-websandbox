//! Core of `tests/unit/runtime/test_files_extra.py` + the grep/glob/replace/view
//! happy paths of `test_files_api.py` — the PR-B-4 file-operation surface.
//!
//! Covers: /ports, /files/view, /files/replace (incl. line-scoped),
//! /download/{path}, /list/{path}, /exists/{path}, /files/grep, /files/glob.

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};

use common::Bearer;

// --- /ports ------------------------------------------------------------------

#[tokio::test]
async fn ports_empty() {
    let env = common::Env::new();
    let resp = env
        .send(Method::GET, "/ports", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body, serde_json::json!({ "ports": [] }));
}

// --- /files/view -------------------------------------------------------------

#[tokio::test]
async fn view_returns_raw_bytes() {
    let env = common::Env::new();
    write_file(&env, "v.bin", "ABCDEF").await;
    let resp = env
        .send(
            Method::GET,
            "/files/view?path=v.bin",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = common::body_bytes(resp).await;
    assert_eq!(&bytes[..], b"ABCDEF");
}

#[tokio::test]
async fn view_missing_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/view?path=ghost",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn view_directory_is_404() {
    let env = common::Env::new();
    mkdir(&env, "adir").await;
    let resp = env
        .send(
            Method::GET,
            "/files/view?path=adir",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// --- /files/replace ----------------------------------------------------------

#[tokio::test]
async fn replace_single() {
    let env = common::Env::new();
    write_file(&env, "r.txt", "hello world").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "r.txt",
                    "replacements": [{"target": "hello", "replacement": "goodbye"}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_content(&env, "r.txt").await, "goodbye world");
}

#[tokio::test]
async fn replace_requires_unique_unless_allow_multiple() {
    let env = common::Env::new();
    write_file(&env, "d.txt", "x x x").await;
    // 3 occurrences, allow_multiple false → 400
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "d.txt",
                    "replacements": [{"target": "x", "replacement": "y"}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // allow_multiple true → 200
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "d.txt",
                    "replacements": [{"target": "x", "replacement": "y", "allow_multiple": true}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_content(&env, "d.txt").await, "y y y");
}

#[tokio::test]
async fn replace_target_not_found() {
    let env = common::Env::new();
    write_file(&env, "n.txt", "abc").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "n.txt",
                    "replacements": [{"target": "zzz", "replacement": "q"}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn replace_directory_is_404() {
    let env = common::Env::new();
    mkdir(&env, "adir").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "adir",
                    "replacements": [{"target": "x", "replacement": "y"}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replace_line_scoped() {
    let env = common::Env::new();
    write_file(&env, "lines.txt", "one\ntwo\nthree").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "lines.txt",
                    "replacements": [{"target": "two", "replacement": "TWO", "start_line": 2, "end_line": 2}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_content(&env, "lines.txt").await, "one\nTWO\nthree");
}

#[tokio::test]
async fn replace_line_scoped_open_end() {
    let env = common::Env::new();
    write_file(&env, "lines.txt", "a\nb\nc\nd").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "lines.txt",
                    "replacements": [{"target": "c", "replacement": "C", "start_line": 3}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_content(&env, "lines.txt").await, "a\nb\nC\nd");
}

#[tokio::test]
async fn replace_line_scoped_inverted_range_noop() {
    let env = common::Env::new();
    write_file(&env, "lines.txt", "one\ntwo\nthree").await;
    let resp = env
        .send(
            Method::POST,
            "/files/replace",
            Bearer::Default,
            None,
            Some(
                serde_json::json!({
                    "path": "lines.txt",
                    "replacements": [{"target": "two", "replacement": "TWO", "start_line": 5, "end_line": 2}]
                })
                .to_string(),
            ),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_content(&env, "lines.txt").await, "one\ntwo\nthree");
}

// --- /download / /list / /exists (LLM-tool surface) --------------------------

#[tokio::test]
async fn download_returns_bytes() {
    let env = common::Env::new();
    write_file(&env, "tool.txt", "tool-payload").await;
    let resp = env
        .send(
            Method::GET,
            "/download/tool.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(common::body_bytes(resp).await, "tool-payload");
}

#[tokio::test]
async fn download_directory_is_404() {
    let env = common::Env::new();
    mkdir(&env, "adir").await;
    let resp = env
        .send(Method::GET, "/download/adir", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_entries_shape() {
    let env = common::Env::new();
    write_file(&env, "a.txt", "aaa").await;
    mkdir(&env, "sub").await;
    let resp = env
        .send(Method::GET, "/list/", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let by_name: std::collections::HashMap<String, serde_json::Value> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e.clone()))
        .collect();
    assert_eq!(by_name["a.txt"]["is_dir"], false);
    assert_eq!(by_name["a.txt"]["size"], 3);
    assert_eq!(by_name["sub"]["is_dir"], true);
}

#[tokio::test]
async fn list_missing_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(Method::GET, "/list/ghost", Bearer::Default, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exists_and_missing() {
    let env = common::Env::new();
    write_file(&env, "tool.txt", "x").await;
    mkdir(&env, "adir").await;
    // file
    let resp = env
        .send(Method::GET, "/exists/tool.txt", Bearer::Default, None, None)
        .await;
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["exists"], true);
    assert_eq!(body["is_file"], true);
    assert_eq!(body["is_dir"], false);
    // dir
    let resp = env
        .send(Method::GET, "/exists/adir", Bearer::Default, None, None)
        .await;
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["is_dir"], true);
    // missing
    let resp = env
        .send(Method::GET, "/exists/ghost", Bearer::Default, None, None)
        .await;
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(
        body,
        serde_json::json!({"exists": false, "is_file": false, "is_dir": false})
    );
}

// --- /files/grep -------------------------------------------------------------

async fn seed_tree(env: &common::Env) {
    write_file(env, "a.txt", "foo bar\nsecond").await;
    write_file(env, "b.txt", "baz foo").await;
    write_file(env, "c.py", "foo = 1").await;
    mkdir(env, "pkg").await;
}

fn split_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[tokio::test]
async fn grep_basic_and_include_filter() {
    let env = common::Env::new();
    seed_tree(&env).await;
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=foo&path=.",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let files: std::collections::HashSet<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| split_name(m["file"].as_str().unwrap()))
        .collect();
    for must in ["a.txt", "b.txt", "c.py"] {
        assert!(files.contains(must), "missing {must}: {files:?}");
    }

    // include filter restricts to *.txt
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=foo&path=.&include=*.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let body: serde_json::Value = common::json(resp).await;
    let files: std::collections::HashSet<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| split_name(m["file"].as_str().unwrap()))
        .collect();
    assert_eq!(
        files,
        ["a.txt", "b.txt"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::HashSet<_>>()
    );
}

#[tokio::test]
async fn grep_literal_mode() {
    let env = common::Env::new();
    write_file(&env, "r.txt", "a.b.c (literal)").await;
    // regex=False matches the literal dot
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=a.b.c&path=.&regex=false",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert!(body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["file"].as_str().unwrap().ends_with("r.txt")));
    // case_insensitive
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=LITERAL&path=.&case_insensitive=true",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let body: serde_json::Value = common::json(resp).await;
    assert!(body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["file"].as_str().unwrap().ends_with("r.txt")));
}

#[tokio::test]
async fn grep_max_results_truncates() {
    let env = common::Env::new();
    for i in 0..10 {
        write_file(&env, &format!("f{i}.txt"), "needle\nneedle").await;
    }
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=needle&path=.&max_results=3",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["truncated"], true);
    assert_eq!(body["matches"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn grep_invalid_regex_is_400() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=(unclosed&path=.",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn grep_missing_path_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=x&path=ghost",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn grep_single_file_path() {
    let env = common::Env::new();
    write_file(&env, "only.txt", "needle here").await;
    write_file(&env, "other.txt", "no match").await;
    let resp = env
        .send(
            Method::GET,
            "/files/grep?query=needle&path=only.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let files: std::collections::HashSet<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| split_name(m["file"].as_str().unwrap()))
        .collect();
    assert_eq!(
        files,
        ["only.txt"]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::HashSet<_>>()
    );
}

// --- /files/glob -------------------------------------------------------------

#[tokio::test]
async fn glob_files_and_dirs() {
    let env = common::Env::new();
    seed_tree(&env).await;
    // files only
    let resp = env
        .send(
            Method::GET,
            "/files/glob?pattern=*.txt&path=.&type=file",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    let names: std::collections::HashSet<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap().to_string())
        .collect();
    for must in ["a.txt", "b.txt"] {
        assert!(names.contains(must), "missing {must}: {names:?}");
    }
    assert!(!names.contains("c.py"));
    // directories only
    let resp = env
        .send(
            Method::GET,
            "/files/glob?pattern=*&path=.&type=directory",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let body: serde_json::Value = common::json(resp).await;
    let dirs: std::collections::HashSet<String> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap().to_string())
        .collect();
    assert!(dirs.contains("pkg"));
    assert!(!dirs.contains("a.txt"));
}

#[tokio::test]
async fn glob_missing_path_is_404() {
    let env = common::Env::new();
    let resp = env
        .send(
            Method::GET,
            "/files/glob?pattern=*&path=ghost",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn glob_max_results_truncates() {
    let env = common::Env::new();
    for i in 0..5 {
        write_file(&env, &format!("f{i}.txt"), "x").await;
    }
    let resp = env
        .send(
            Method::GET,
            "/files/glob?pattern=*.txt&path=.&max_results=1",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = common::json(resp).await;
    assert_eq!(body["truncated"], true);
    assert_eq!(body["matches"].as_array().unwrap().len(), 1);
}

// --- helpers -----------------------------------------------------------------

async fn write_file(env: &common::Env, path: &str, content: &str) {
    let body = serde_json::json!({ "path": path, "content": content }).to_string();
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            None,
            Some(body),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn mkdir(env: &common::Env, path: &str) {
    let body = serde_json::json!({ "path": path }).to_string();
    let resp = env
        .send(
            Method::POST,
            "/files/mkdir",
            Bearer::Default,
            None,
            Some(body),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn read_content(env: &common::Env, path: &str) -> String {
    let resp = env
        .send(
            Method::GET,
            &format!("/files/read?path={path}"),
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    common::json::<serde_json::Value>(resp).await["content"]
        .as_str()
        .unwrap()
        .to_string()
}
