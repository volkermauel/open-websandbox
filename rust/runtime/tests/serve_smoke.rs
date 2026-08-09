//! TCP-level smoke test of the live `axum::serve` wiring.
//!
//! The in-process `tower::ServiceExt::oneshot` contract tests cover routing and
//! handlers; this one proves the full network stack the `runtime` binary uses —
//! `TcpListener` bind → `axum::serve` accept → HTTP/1.1 parse → router → handler
//! → response — by binding an OS-assigned ephemeral port (so it never collides
//! with the production 8888 held by a runtime pod on a shared host) and sending
//! raw HTTP requests over a real socket.

#![forbid(unsafe_code)]

mod common;

use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};

/// Send a raw HTTP/1.1 request over a TCP socket and return `(status, body)`.
async fn http(port: u16, request: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(request).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split("\r\n")
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_over_tcp() {
    let tmp = tempfile::TempDir::new().unwrap();
    let workdir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workdir).unwrap();
    let key_path = tmp.path().join("api-key");
    std::fs::write(&key_path, common::STRONG_KEY).unwrap();
    let config = RuntimeConfig {
        workdir: workdir.clone(),
        runtime_key_file: key_path.clone(),
        shell: "/bin/sh".to_string(),
        ..RuntimeConfig::default()
    };
    let key_store = SessionKeyStore::new(&key_path);
    let app = build_router(AppState::new(config, key_store));

    // OS-assigned free port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Open health route returns the exact contract body.
    let (status, body) = http(
        port,
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, r#"{"status":"ok","runtime":"code-standard"}"#);

    // Open probes.
    let (status, _) = http(
        port,
        b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = http(
        port,
        b"GET /readyz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);

    // Gated route rejects without a Bearer (401).
    let (status, _) = http(
        port,
        b"POST /execute HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    )
    .await;
    assert_eq!(status, 401);

    // Gated route accepts with the per-session key and runs a command (200).
    let req = format!(
        "POST /execute HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
        key = common::STRONG_KEY,
        len = r#"{"command":"echo hi","timeout":5}"#.len(),
        body = r#"{"command":"echo hi","timeout":5}"#
    );
    let (status, body) = http(port, req.as_bytes()).await;
    assert_eq!(status, 200);
    assert!(body.contains(r#""stdout":"hi\n""#), "{body}");
    assert!(body.contains(r#""exit_code":0"#), "{body}");
}
