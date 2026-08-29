//! Contract tests for the open-terminal v0.12.3 stage-1 compatibility surface
//! (#164): `GET /files/serve/{path}`, `GET /api/config`, `/files/list`
//! writability flags (0.11.35), `/files/read` line ranges + binary 415
//! (0.2.7), `GET /files/search` (0.11.36), `GET /files/matches` (0.12.0),
//! `GET /files/cwd` `root` for FileNav (#179).

#![forbid(unsafe_code)]

mod common;

use axum::http::{Method, StatusCode};
use common::{json, status, Bearer};
use serde_json::Value;

use common::Env;

/// Write a text file through the API (keeps every test on the HTTP surface).
async fn put(env: &Env, path: &str, content: &str) {
    let resp = env
        .send(
            Method::POST,
            "/files/write",
            Bearer::Default,
            None,
            Some(format!(
                "{{\"path\": {path}, \"content\": {content}}}",
                path = serde_json::to_string(path).unwrap(),
                content = serde_json::to_string(content).unwrap(),
            )),
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK, "write {path}");
}

// --- GET /api/config ---------------------------------------------------------

#[tokio::test]
async fn api_config_is_unauthenticated_feature_discovery() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", Bearer::None, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    assert_eq!(
        doc,
        // `system` flipped true in stage 2 (#169): GET /system now exists.
        serde_json::json!({
            "features": {"terminal": true, "notebooks": false, "system": true}
        })
    );
}

// --- GET /files/serve/{path} --------------------------------------------------

#[tokio::test]
async fn serve_returns_inline_bytes_with_content_type() {
    let env = Env::new();
    put(&env, "site/index.html", "<h1>hi</h1>").await;

    let resp = env
        .send(
            Method::GET,
            "/files/serve/site/index.html",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html")
    );
    // Inline serving: no attachment disposition (unlike /files/view).
    assert!(resp.headers().get("content-disposition").is_none());
    assert_eq!(common::body_bytes(resp).await, "<h1>hi</h1>".as_bytes());
}

#[tokio::test]
async fn serve_requires_auth_and_a_real_file() {
    let env = Env::new();
    let unauth = env
        .send(Method::GET, "/files/serve/x.txt", Bearer::None, None, None)
        .await;
    assert_eq!(status(&unauth), StatusCode::UNAUTHORIZED);
    let missing = env
        .send(
            Method::GET,
            "/files/serve/missing.txt",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&missing), StatusCode::NOT_FOUND);
    let escape = env
        .send(
            Method::GET,
            "/files/serve/../../etc/passwd",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&escape), StatusCode::BAD_REQUEST);
}

// --- GET /files/cwd `root` (FileNav parity, #179) -----------------------------

#[tokio::test]
async fn cwd_carries_file_nav_root() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/files/cwd", Bearer::Default, None, None)
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    // OWUI FileNav roots its tree at `root ?? "/"`; an absent root made it
    // list `/`, which the safe-path jail answers 400 (#179).
    assert!(doc.get("root").is_some(), "root key must be present");
    let base = env.workdir.to_str().unwrap();
    assert_eq!(doc["root"], base);
    assert_eq!(doc["cwd"], base);
    assert_eq!(doc["home"], base);
}

// --- /files/list writability (0.11.35) ----------------------------------------

#[tokio::test]
async fn list_carries_writable_flags() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    put(&env, "writable.txt", "x").await;
    put(&env, "locked.txt", "y").await;
    let locked = env.workdir.join("locked.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();

    let resp = env
        .send(
            Method::GET,
            "/files/list?directory=.",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    assert_eq!(doc["writable"], serde_json::json!(true), "dir is writable");
    let entries: Vec<&Value> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| ["writable.txt", "locked.txt"].contains(&e["name"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(entries.len(), 2);
    for e in entries {
        let expect = e["name"].as_str().unwrap() != "locked.txt";
        // chmod 444 must show writable=false — unless the test host runs as
        // root, where access(W_OK) always succeeds (gVisor/rooted CI).
        if !(expect && nix::unistd::geteuid().is_root()) {
            assert_eq!(e["writable"].as_bool().unwrap(), expect, "{}", e["name"]);
        }
    }
}

// --- /files/read line ranges + 415 (0.2.7) -------------------------------------

#[tokio::test]
async fn read_slices_one_indexed_inclusive_line_ranges() {
    let env = Env::new();
    put(&env, "lines.txt", "one\ntwo\nthree\nfour\n").await;

    let resp = env
        .send(
            Method::GET,
            "/files/read?path=lines.txt&start_line=2&end_line=3",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    assert_eq!(doc["total_lines"], serde_json::json!(4));
    assert_eq!(doc["content"], serde_json::json!("two\nthree\n"));

    let zero = env
        .send(
            Method::GET,
            "/files/read?path=lines.txt&start_line=0",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&zero), StatusCode::BAD_REQUEST);

    let over = env
        .send(
            Method::GET,
            "/files/read?path=lines.txt&start_line=10&end_line=12",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&over), StatusCode::OK);
    let doc: Value = json(over).await;
    assert_eq!(doc["content"], serde_json::json!(""));
}

#[tokio::test]
async fn read_rejects_non_image_binaries_with_415() {
    let env = Env::new();
    // Binary, non-UTF-8, non-image: a gzip magic header followed by NULs.
    let mut payload = vec![0x1f, 0x8b, 0x08, 0x00];
    payload.extend(std::iter::repeat_n(0u8, 64));
    let (ct, body) = gzip_multipart("blob.bin", &payload);
    let up = env
        .send_typed(
            Method::POST,
            "/files/upload?directory=.",
            Bearer::Default,
            None,
            &ct,
            body,
        )
        .await;
    assert_eq!(status(&up), StatusCode::OK, "upload binary");

    let resp = env
        .send(
            Method::GET,
            "/files/read?path=blob.bin",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let text = common::body_text(resp).await;
    assert!(
        text.contains("Unsupported binary file type"),
        "detail carries the upstream-style message: {text}"
    );
}

/// Minimal single-file multipart body for the binary upload.
fn gzip_multipart(filename: &str, content: &[u8]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "----owui-compat-boundary";
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

// --- GET /files/search (0.11.36) -----------------------------------------------

#[tokio::test]
async fn search_ranks_filters_and_hides_dotfiles() {
    let env = Env::new();
    put(&env, "needle.txt", "n1").await;
    put(&env, "needle_v2.txt", "n2").await;
    put(&env, "contains-NEEDLE-inside.txt", "n3").await;
    put(&env, "sub/needle_child.txt", "n4").await;
    put(&env, ".needle_hidden.txt", "n5").await;

    let resp = env
        .send(
            Method::GET,
            "/files/search?query=needle",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    let names: Vec<String> = doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    // rank 0 (exact) → rank 1 (prefix) → rank 2 (substring); at equal rank
    // the shorter NAME wins (upstream sort `(rank, len(name), relpath)`), so
    // needle_child.txt precedes contains-NEEDLE-inside.txt. Hidden excluded.
    assert_eq!(
        names,
        [
            "needle.txt",
            "needle_v2.txt",
            "needle_child.txt",
            "contains-NEEDLE-inside.txt"
        ]
    );
    for r in doc["results"].as_array().unwrap() {
        assert!(
            r["path"].as_str().unwrap().starts_with('/'),
            "absolute paths"
        );
    }

    // type=directory: only the `sub` dir matches "needle"? No — it must match
    // by NAME, so a directory named after the query.
    put(&env, "needle_dir/.keep", "k").await;
    let resp = env
        .send(
            Method::GET,
            "/files/search?query=needle_dir&type=directory",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let doc: Value = json(resp).await;
    let names: Vec<&str> = doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["needle_dir"]);

    // show_hidden surfaces dotfiles.
    let resp = env
        .send(
            Method::GET,
            "/files/search?query=needle_hidden&show_hidden=true",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let doc: Value = json(resp).await;
    assert_eq!(doc["results"].as_array().unwrap().len(), 1);

    // limit bounds are validated.
    let bad = env
        .send(
            Method::GET,
            "/files/search?query=n&limit=0",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&bad), StatusCode::BAD_REQUEST);
    let bad = env
        .send(
            Method::GET,
            "/files/search?query=n&type=symlink",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&bad), StatusCode::BAD_REQUEST);
}

// --- GET /files/matches (0.12.0) ------------------------------------------------

#[tokio::test]
async fn matches_unifies_name_and_content_with_pagination() {
    let env = Env::new();
    put(
        &env,
        "zebra.txt",
        "the quick zebra\njumps\nover the lazy zebra\n",
    )
    .await;
    put(&env, "zebra.png", "\u{fffd}binary-ish\u{fffd}").await;
    put(&env, "unrelated.md", "nothing here\nmentions the animal\n").await;

    let resp = env
        .send(
            Method::GET,
            "/files/matches?query=zebra",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&resp), StatusCode::OK);
    let doc: Value = json(resp).await;
    let results = doc["results"].as_array().unwrap();
    // Both names merely *start with* the query (score 1: neither file is
    // literally named "zebra"), so the tie-break is `(score, rel.len(),
    // rel.lower)` → zebra.png before zebra.txt. zebra.png is name-only; the
    // .txt carries the content matches. unrelated.md absent.
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"], serde_json::json!("zebra.png"));
    assert!(results[0]["name_match"].as_bool().unwrap());
    assert!(results[0]["content_matches"].as_array().unwrap().is_empty());
    assert_eq!(results[1]["name"], serde_json::json!("zebra.txt"));
    assert!(results[1]["name_match"].as_bool().unwrap());
    let hits = results[1]["content_matches"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "one per line, <=3 per file");
    assert_eq!(hits[0]["line"], serde_json::json!(1));
    assert_eq!(hits[0]["column"], serde_json::json!(11), "utf-16 columns");
    assert_eq!(hits[0]["text"], serde_json::json!("the quick zebra"));

    // Content-only match: name doesn't contain the query.
    put(&env, "doc.md", "a xyzzy plugh\n").await;
    let resp = env
        .send(
            Method::GET,
            "/files/matches?query=xyzzy",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let doc: Value = json(resp).await;
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], serde_json::json!("doc.md"));
    assert!(!results[0]["name_match"].as_bool().unwrap());
    assert_eq!(
        results[0]["content_matches"][0]["column"],
        serde_json::json!(3)
    );

    // Pagination: one result per page, next_offset chains to the end.
    let resp = env
        .send(
            Method::GET,
            "/files/matches?query=zebra&limit=1",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let doc: Value = json(resp).await;
    assert_eq!(doc["results"].as_array().unwrap().len(), 1);
    assert_eq!(doc["next_offset"], serde_json::json!(1));
    let resp = env
        .send(
            Method::GET,
            "/files/matches?query=zebra&limit=1&offset=1",
            Bearer::Default,
            None,
            None,
        )
        .await;
    let doc: Value = json(resp).await;
    assert_eq!(doc["results"].as_array().unwrap().len(), 1);
    assert_eq!(doc["next_offset"], serde_json::json!(null));

    // Blank query → 400 (upstream behavior).
    let blank = env
        .send(
            Method::GET,
            "/files/matches?query=%20%20",
            Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(status(&blank), StatusCode::BAD_REQUEST);
}
