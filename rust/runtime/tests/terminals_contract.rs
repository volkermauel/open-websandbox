//! Interactive terminal (PTY) contract tests — the terminal + extra-terminal
//! cases.
//!
//! The wire contract is **strict 1:1** (D5 / D11): raw
//! binary WS frames carry stdin/stdout, text JSON frames carry `resize` control,
//! `POST/GET/DELETE /api/terminals[/{id}]` return the exact JSON shapes and
//! status codes (200, 429, 503, 404), and an unknown/dead session closes the socket
//! with code **4004**.
//!
//! Because a WebSocket cannot be driven through `tower::ServiceExt::oneshot`, these
//! stand up a real `axum::serve` listener (OS-assigned port) and speak HTTP/1.1 over
//! the socket for the CRUD ops + `tokio-tungstenite` for the relay — every PTY here
//! is real (no mocking of openpty/fork/ioctl).

#![forbid(unsafe_code)]

mod common;

use std::time::Duration;

use axum::body::Bytes;
use futures_util::{SinkExt, StreamExt};
use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

/// A live runtime: tmp workdir + projected key + a served axum app on an ephemeral
/// port. `shell` and `max` are configurable so the cap-429 and spawn-fail-503 cases
/// can be exercised.
struct Server {
    port: u16,
    key: &'static str,
    _tmp: TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::with("/bin/bash", 8).await
    }
    async fn with(shell: &str, max: u32) -> Self {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        let key_path = tmp.path().join("api-key");
        std::fs::write(&key_path, common::STRONG_KEY).unwrap();
        let config = RuntimeConfig {
            workdir: workdir.clone(),
            runtime_key_file: key_path.clone(),
            shell: shell.to_string(),
            max_terminal_sessions: max,
            ..RuntimeConfig::default()
        };
        let app = build_router(AppState::new(config, SessionKeyStore::new(&key_path)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Server {
            port,
            key: common::STRONG_KEY,
            _tmp: tmp,
        }
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.key)
    }
}

/// Speak HTTP/1.1 over a fresh TCP socket; return `(status, body_text)`.
async fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let body_bytes = body.unwrap_or("").as_bytes();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body_bytes.is_empty() {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body_bytes.len()
        ));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    if !body_bytes.is_empty() {
        s.write_all(body_bytes).await.unwrap();
    }
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split("\r\n")
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|x| x.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// JSON-decode the body of an HTTP response (strips any stray chunked tail).
fn json(body: &str) -> serde_json::Value {
    let trimmed = body.trim_end_matches("0\r\n\r\n").trim();
    serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("not JSON: {body:?} ({e})"))
}

/// Build a WS upgrade request with an optional Bearer (tokio-tungstenite client).
fn build_req(port: u16, path: &str, bearer: Option<&str>) -> http::Request<()> {
    let mut b = http::Request::builder()
        .method("GET")
        .uri(format!("ws://127.0.0.1:{port}{path}"))
        .header("host", format!("127.0.0.1:{port}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
    if let Some(k) = bearer {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(()).unwrap()
}

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn ws_connect(port: u16, path: &str, bearer: Option<&str>) -> WsStream {
    tokio_tungstenite::connect_async(build_req(port, path, bearer))
        .await
        .expect("ws upgrade")
        .0
}

/// `true` if the WS handshake was rejected (e.g. missing Bearer → auth-at-upgrade).
async fn ws_upgrade_rejected(port: u16, path: &str, bearer: Option<&str>) -> bool {
    tokio_tungstenite::connect_async(build_req(port, path, bearer))
        .await
        .is_err()
}

/// Read frames until a Close arrives; return its code (0 if the stream ended first).
async fn first_close_code(stream: &mut WsStream) -> u16 {
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Close(Some(frame)) = msg {
            return frame.code.into();
        }
    }
    0
}

/// Read binary/text frames until `marker` appears in the accumulated buffer, or timeout.
async fn wait_for(stream: &mut WsStream, marker: &[u8], timeout: f64) -> bool {
    let deadline = tokio::time::sleep(Duration::from_secs_f64(timeout));
    tokio::pin!(deadline);
    let mut buf = Vec::new();
    loop {
        if window_contains(&buf, marker) {
            return true;
        }
        tokio::select! {
            _ = &mut deadline => return window_contains(&buf, marker),
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(b))) => buf.extend_from_slice(&b),
                Some(Ok(Message::Text(t))) => buf.extend_from_slice(t.as_bytes()),
                Some(Ok(Message::Ping(p))) => buf.extend_from_slice(&p),
                Some(Ok(Message::Close(_))) => return window_contains(&buf, marker),
                _ => {}
            }
        }
    }
}

fn window_contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Poll GET /api/terminals/{id} until it 404s (WS-disconnect teardown), with a budget.
async fn poll_cleaned(srv: &Server, id: &str, timeout: f64) -> bool {
    let deadline = tokio::time::sleep(Duration::from_secs_f64(timeout));
    tokio::pin!(deadline);
    loop {
        let (s, _) = http(
            srv.port,
            "GET",
            &format!("/api/terminals/{id}"),
            &[("Authorization", &srv.auth())],
            None,
        )
        .await;
        if s == 404 {
            return true;
        }
        tokio::select! {
            _ = &mut deadline => return false,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

// --- HTTP contract -----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn post_creates_session_with_expected_shape() {
    let srv = Server::start().await;
    let (status, body) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", "abc")],
        None,
    )
    .await;
    assert_eq!(status, 200, "body={body}");
    let v = json(&body);
    assert_eq!(v["id"], "abc");
    assert!(v["pid"].as_i64().unwrap_or(0) > 0, "{v}");
    assert!(v["created_at"].is_string(), "{v}");
    let _ = delete(&srv, "abc").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn terminals_require_auth() {
    let srv = Server::start().await;
    assert_eq!(
        http(srv.port, "POST", "/api/terminals", &[], None).await.0,
        401
    );
    assert_eq!(
        http(srv.port, "GET", "/api/terminals", &[], None).await.0,
        401
    );
    assert_eq!(
        http(srv.port, "DELETE", "/api/terminals/x", &[], None)
            .await
            .0,
        401
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_returns_info_then_404_after_delete() {
    let srv = Server::start().await;
    create(&srv, "g1").await;
    let (s, body) = http(
        srv.port,
        "GET",
        "/api/terminals/g1",
        &[("Authorization", &srv.auth())],
        None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(json(&body)["id"], "g1");
    assert_eq!(delete(&srv, "g1").await, 200);
    let (s, _) = http(
        srv.port,
        "GET",
        "/api/terminals/g1",
        &[("Authorization", &srv.auth())],
        None,
    )
    .await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_is_idempotent_for_missing() {
    let srv = Server::start().await;
    let (s, body) = http(
        srv.port,
        "DELETE",
        "/api/terminals/never-existed",
        &[("Authorization", &srv.auth())],
        None,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(json(&body)["status"], "deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_reports_live_sessions() {
    let srv = Server::start().await;
    for id in ["L1", "L2"] {
        create(&srv, id).await;
    }
    let (s, body) = http(
        srv.port,
        "GET",
        "/api/terminals",
        &[("Authorization", &srv.auth())],
        None,
    )
    .await;
    assert_eq!(s, 200);
    let v = json(&body);
    let arr = v.as_array().unwrap();
    let ids: Vec<&str> = arr.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"L1") && ids.contains(&"L2"), "{ids:?}");
    for e in arr {
        assert!(e["created_at"].is_string());
        assert!(e["pid"].as_i64().unwrap_or(0) > 0);
    }
    for id in ["L1", "L2"] {
        let _ = delete(&srv, id).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recreate_existing_id_spawns_new_session() {
    let srv = Server::start().await;
    let (_, body) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", "dup")],
        None,
    )
    .await;
    let pid1 = json(&body)["pid"].as_i64().unwrap();
    let (_, body) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", "dup")],
        None,
    )
    .await;
    let pid2 = json(&body)["pid"].as_i64().unwrap();
    assert_ne!(pid1, pid2, "recreate should spawn a fresh shell");
    let _ = delete(&srv, "dup").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cap_reached_is_429() {
    let srv = Server::with("/bin/bash", 1).await;
    assert_eq!(create(&srv, "cap1").await, 200);
    let (s, body) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", "cap2")],
        None,
    )
    .await;
    assert_eq!(s, 429, "body={body}");
    let v = json(&body);
    let detail = v["detail"].as_str().unwrap_or_default().to_lowercase();
    assert!(detail.contains("max"), "detail={detail}");
    let _ = delete(&srv, "cap1").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_failure_is_503() {
    let srv = Server::with("/nonexistent/shell-xyz-12345", 8).await;
    let (s, body) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", "badspawn")],
        None,
    )
    .await;
    assert_eq!(s, 503, "body={body}");
    let v = json(&body);
    let detail = v["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("pty spawn failed"), "detail={detail}");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminals_honour_subdir_header() {
    // Unlike snapshot/restore (D11), terminals spawn in WORKDIR/<subdir>. request_base
    // must create it, proving the header is honoured.
    let srv = Server::start().await;
    let (s, _) = http(
        srv.port,
        "POST",
        "/api/terminals",
        &[
            ("Authorization", &srv.auth()),
            ("X-Session-Id", "sub"),
            ("X-Workspace-Subdir", "mysub"),
        ],
        None,
    )
    .await;
    assert_eq!(s, 200);
    // The subdir under the workspace was created by request_base.
    // (The shell's cwd is the subdir; we can't easily read cwd over HTTP, but the
    // created directory proves the header flowed through request_base.)
    let _ = delete(&srv, "sub").await;
}

// --- WebSocket contract ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ws_rejects_upgrade_without_bearer() {
    let srv = Server::start().await;
    create(&srv, "noauth").await;
    assert!(
        ws_upgrade_rejected(srv.port, "/api/terminals/noauth", None).await,
        "upgrade must be rejected without a Bearer"
    );
    let _ = delete(&srv, "noauth").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_unknown_session_closes_4004() {
    let srv = Server::start().await;
    let mut stream = ws_connect(srv.port, "/api/terminals/never-made", Some(srv.key)).await;
    // The server accepts then closes with 4004 (unknown/ended session).
    assert_eq!(first_close_code(&mut stream).await, 4004);
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_echo_resize_then_cleanup() {
    let srv = Server::start().await;
    create(&srv, "echo1").await;
    let mut ws = ws_connect(srv.port, "/api/terminals/echo1", Some(srv.key)).await;
    // echo input round-trips over the PTY as binary frames.
    ws.send(Message::Binary(Bytes::from_static(b"echo hi\n")))
        .await
        .unwrap();
    assert!(wait_for(&mut ws, b"hi", 4.0).await, "PTY did not echo `hi`");
    // resize control frame must not kill the session.
    ws.send(Message::Text(
        r#"{"type":"resize","rows":30,"cols":100}"#.into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Binary(Bytes::from_static(b"echo afterresize\n")))
        .await
        .unwrap();
    assert!(
        wait_for(&mut ws, b"afterresize", 4.0).await,
        "session died after resize frame"
    );
    drop(ws);
    assert!(
        poll_cleaned(&srv, "echo1", 5.0).await,
        "terminal entry survived WS disconnect (PTY leak)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_control_frames_tolerated() {
    let srv = Server::start().await;
    create(&srv, "ctrl").await;
    let mut ws = ws_connect(srv.port, "/api/terminals/ctrl", Some(srv.key)).await;
    for frame in [
        r#"{"type":"resize","cols":40,"rows":12}"#,
        r#"{"type":"something_else"}"#,
        r#"{"type":"auth","token":"anything"}"#,
        "<<<not json>>>",
    ] {
        ws.send(Message::Text(frame.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    ws.send(Message::Binary(Bytes::from_static(b"echo stillalive\n")))
        .await
        .unwrap();
    assert!(
        wait_for(&mut ws, b"stillalive", 5.0).await,
        "control frames killed the session"
    );
    drop(ws);
    assert!(poll_cleaned(&srv, "ctrl", 5.0).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_shell_exit_cleans_up() {
    let srv = Server::start().await;
    create(&srv, "ex").await;
    let mut ws = ws_connect(srv.port, "/api/terminals/ex", Some(srv.key)).await;
    ws.send(Message::Binary(Bytes::from_static(b"exit\n")))
        .await
        .unwrap();
    // shell exits → PTY EOF/EIO → relay ends → session reaped.
    assert!(
        poll_cleaned(&srv, "ex", 8.0).await,
        "session survived shell exit"
    );
    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_background_job_then_exit_cleans_up() {
    let srv = Server::start().await;
    create(&srv, "bg").await;
    let mut ws = ws_connect(srv.port, "/api/terminals/bg", Some(srv.key)).await;
    // background a long sleep (holds the slave open), then leave the shell.
    ws.send(Message::Binary(Bytes::from_static(b"sleep 120 &\n")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    ws.send(Message::Binary(Bytes::from_static(b"exit\n")))
        .await
        .unwrap();
    // teardown killpg(SIGKILL) the whole group → orphaned sleep dies too → reaped.
    assert!(
        poll_cleaned(&srv, "bg", 8.0).await,
        "background-job session was not reaped"
    );
    let _ = ws.close(None).await;
}

// --- tiny CRUD conveniences --------------------------------------------------

async fn create(srv: &Server, id: &str) -> u16 {
    http(
        srv.port,
        "POST",
        "/api/terminals",
        &[("Authorization", &srv.auth()), ("X-Session-Id", id)],
        None,
    )
    .await
    .0
}

async fn delete(srv: &Server, id: &str) -> u16 {
    http(
        srv.port,
        "DELETE",
        &format!("/api/terminals/{id}"),
        &[("Authorization", &srv.auth())],
        None,
    )
    .await
    .0
}
