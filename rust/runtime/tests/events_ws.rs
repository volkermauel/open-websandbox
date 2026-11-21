//! `WS /events` contract tests (stage 3) — the upstream fs/ports change
//! stream: subscribe validation closes 4000, an escaping watch path closes
//! 4003, `fs:ready`/`fs:change` fire on fingerprint deltas, and
//! `ports:change` carries the visible-port snapshot. A missing Bearer is
//! rejected at the upgrade (401), like every runtime route.
//!
//! Real `axum::serve` listener + `tokio-tungstenite` client (same harness as
//! `terminals_contract.rs`).

#![forbid(unsafe_code)]

mod common;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A live runtime: tmp workdir + projected key + a served axum app on an
/// ephemeral port.
struct Server {
    port: u16,
    key: &'static str,
    workdir: std::path::PathBuf,
    _tmp: TempDir,
}

impl Server {
    async fn start() -> Self {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workdir).unwrap();
        let key_path = tmp.path().join("api-key");
        std::fs::write(&key_path, common::STRONG_KEY).unwrap();
        let config = RuntimeConfig {
            workdir: workdir.clone(),
            runtime_key_file: key_path.clone(),
            ..RuntimeConfig::default()
        };
        let state = AppState::new(config, SessionKeyStore::new(&key_path));
        let app = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Server {
            port,
            key: common::STRONG_KEY,
            workdir,
            _tmp: tmp,
        }
    }
}

/// Build an upgrade request with an optional Bearer.
fn build_req(
    port: u16,
    path: &str,
    bearer: Option<&str>,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut req = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    if let Some(b) = bearer {
        use tokio_tungstenite::tungstenite::http::header;
        req.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {b}").parse().unwrap(),
        );
    }
    req
}

async fn ws_connect(port: u16, path: &str, bearer: Option<&str>) -> WsStream {
    tokio_tungstenite::connect_async(build_req(port, path, bearer))
        .await
        .expect("ws upgrade")
        .0
}

/// Collect TEXT JSON frames until `type` matches, or timeout. Returns the frame.
async fn wait_for_type(stream: &mut WsStream, kind: &str, timeout: Duration) -> Option<Message> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if v.get("type").and_then(|t| t.as_str()) == Some(kind) {
                        return Some(Message::Text(text));
                    }
                }
            }
            _ => return None,
        }
    }
}

/// Read frames until a Close arrives; return its code (0 if the stream ended).
async fn first_close_code(stream: &mut WsStream) -> u16 {
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Close(Some(frame)) = msg {
            return frame.code.into();
        }
    }
    0
}

#[tokio::test]
async fn upgrade_requires_the_runtime_key() {
    let srv = Server::start().await;
    let rejected = tokio_tungstenite::connect_async(build_req(srv.port, "/events", None)).await;
    let Err(e) = rejected else {
        panic!("no-bearer upgrade must be rejected");
    };
    // 401 surfaces as a failed handshake carrying the HTTP status.
    let status = match &e {
        tokio_tungstenite::tungstenite::Error::Http(resp) => resp.status(),
        _ => panic!("expected an HTTP rejection, got {e}"),
    };
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_frame_is_tolerated_then_bad_subscribe_closes_4000() {
    let srv = Server::start().await;
    let mut ws = ws_connect(srv.port, "/events", Some(srv.key)).await;
    // The broker forwards OWUI's auth frame — tolerated (ignored).
    ws.send(Message::Text(
        r#"{"type":"auth","token":"","chat_id":"c1"}"#.into(),
    ))
    .await
    .unwrap();
    // A non-subscribe frame afterwards closes 4000 (upstream parity).
    ws.send(Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    assert_eq!(first_close_code(&mut ws).await, 4000);
}

#[tokio::test]
async fn escaping_watch_path_closes_4003() {
    let srv = Server::start().await;
    let mut ws = ws_connect(srv.port, "/events", Some(srv.key)).await;
    ws.send(Message::Text(r#"{"type":"auth","token":""}"#.into()))
        .await
        .unwrap();
    ws.send(Message::Text(
        r#"{"type":"subscribe","path":"../.."}"#.into(),
    ))
    .await
    .unwrap();
    assert_eq!(first_close_code(&mut ws).await, 4003);
}

#[tokio::test]
async fn fs_ready_then_change_on_file_mutation() {
    let srv = Server::start().await;
    let mut ws = ws_connect(srv.port, "/events", Some(srv.key)).await;
    ws.send(Message::Text(r#"{"type":"auth","token":""}"#.into()))
        .await
        .unwrap();
    ws.send(Message::Text(r#"{"type":"subscribe","path":"."}"#.into()))
        .await
        .unwrap();
    // fs:ready arrives right after the watcher installs.
    assert!(
        wait_for_type(&mut ws, "fs:ready", Duration::from_secs(5))
            .await
            .is_some(),
        "fs:ready missing"
    );
    // A file mutation must fire fs:change within the poll cadence + slack.
    std::fs::write(srv.workdir.join("mutated.txt"), b"change").unwrap();
    assert!(
        wait_for_type(&mut ws, "fs:change", Duration::from_secs(5))
            .await
            .is_some(),
        "fs:change missing after file write"
    );
    // A git marker change fires too (the .git/HEAD key is in the fingerprint).
    let git = srv.workdir.join(".git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    assert!(
        wait_for_type(&mut ws, "fs:change", Duration::from_secs(5))
            .await
            .is_some(),
        "fs:change missing after git HEAD write"
    );
}

#[tokio::test]
async fn ports_subscription_pushes_the_snapshot() {
    let srv = Server::start().await;
    let mut ws = ws_connect(srv.port, "/events", Some(srv.key)).await;
    ws.send(Message::Text(r#"{"type":"auth","token":""}"#.into()))
        .await
        .unwrap();
    ws.send(Message::Text(r#"{"type":"subscribe","ports":true}"#.into()))
        .await
        .unwrap();
    let frame = wait_for_type(&mut ws, "ports:change", Duration::from_secs(5)).await;
    let Some(Message::Text(text)) = frame else {
        panic!("ports:change snapshot missing");
    };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        v.get("ports").is_some_and(serde_json::Value::is_array),
        "{text}"
    );
}
