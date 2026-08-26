//! Terminal WebSocket relay integration test (issue #101).
//!
//! Exercises [`broker::terminal::relay_to_upstream`] end-to-end against a tiny
//! in-process `tokio-tungstenite` *echo* server. The echo server stands in for
//! the runtime pod's `ws://<pod-ip>:8888/api/terminals/{id}` endpoint (which a
//! real e2e would need deployed): frames a test WS client sends to a local axum
//! endpoint — which routes through the broker's relay + the `AxumMsg`↔`TungMsg`
//! frame translation — come back verbatim. No Kubernetes / runtime pod required.
//!
//! **Env-gated** by `OWUI_WS_LIVE=1` (any of `1`/`true`/`yes`/`on`): the test
//! binds real loopback listeners, so a plain `cargo test --workspace` (no
//! network opt-in) skips it. Run locally:
//!
//! ```text
//! OWUI_WS_LIVE=1 cargo test -p broker --test ws_relay -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use broker::terminal::relay_to_upstream;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TungMsg;

/// Run only when the operator opted in (`OWUI_WS_LIVE=1`); otherwise every test
/// returns (passes) so a plain `cargo test` needs no loopback listener.
fn gated() -> bool {
    std::env::var("OWUI_WS_LIVE").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// A minimal WS echo server: every Text/Binary frame received is sent straight
/// back. Stands in for the runtime pod's terminal WS. Runs until the listener is
/// dropped (test teardown).
async fn echo_server(listener: TcpListener) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    // Echo data frames verbatim; let the libraries own control frames.
                    m @ (TungMsg::Text(_) | TungMsg::Binary(_)) => {
                        if ws.send(m).await.is_err() {
                            break;
                        }
                    }
                    TungMsg::Close(_) => break,
                    TungMsg::Ping(_) | TungMsg::Pong(_) | TungMsg::Frame(_) => {}
                }
            }
        });
    }
}

/// Axum handler: upgrade the WS and hand the socket to the broker relay pointed
/// at the echo server (URL passed via `State`).
async fn relay_handler(ws: WebSocketUpgrade, State(echo_url): State<String>) -> Response {
    // `on_upgrade` needs a `'static` future; own the URL inside the async block
    // so the `&str` borrow lives as long as the relay future.
    ws.on_upgrade(move |socket| {
        let url = echo_url;
        async move { relay_to_upstream(socket, &url).await }
    })
}

/// Stand up the echo server + an axum relay endpoint pointed at it, and return
/// the relay URL a test client should connect to. Both run on ephemeral
/// loopback ports (bound before serving, so the OS backlog absorbs the client's
/// first SYN — no readiness race).
async fn stand_up_relay() -> String {
    let echo_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo listener");
    let echo_addr = echo_listener.local_addr().expect("echo local addr");
    tokio::spawn(echo_server(echo_listener));

    let app = Router::new()
        .route("/term", get(relay_handler))
        .with_state(format!("ws://{echo_addr}/api/terminals/t1"));
    let relay_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay listener");
    let relay_addr = relay_listener.local_addr().expect("relay local addr");
    tokio::spawn(async move {
        // Serve until the process ends; the listener owns the port for the test.
        let _ = axum::serve(relay_listener, app).await;
    });
    format!("ws://{relay_addr}/term")
}

/// Text and binary frames a client sends round-trip through the broker relay
/// (client→upstream via `to_upstream`, upstream→client via `to_client`) and the
/// echo server. This is the real byte-relay path `relay` runs in production.
#[tokio::test]
async fn relay_forwards_text_and_binary_both_ways() {
    if !gated() {
        eprintln!("skipped: set OWUI_WS_LIVE=1 to run the WS relay test");
        return;
    }
    let url = stand_up_relay().await;
    let (mut client, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect to relay");

    client
        .send(TungMsg::Text("hello-terminal".into()))
        .await
        .expect("send text");
    client
        .send(TungMsg::Binary(vec![1, 2, 3, 4].into()))
        .await
        .expect("send binary");

    let mut got_text = false;
    let mut got_bin = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !(got_text && got_bin) {
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(1), client.next()).await
        {
            match msg {
                TungMsg::Text(t) => {
                    assert_eq!(t.as_str(), "hello-terminal", "text echoed verbatim");
                    got_text = true;
                }
                TungMsg::Binary(b) => {
                    assert_eq!(b.as_ref(), &[1, 2, 3, 4], "binary echoed verbatim");
                    got_bin = true;
                }
                _ => {}
            }
        }
    }
    assert!(got_text, "text frame round-tripped through the relay");
    assert!(got_bin, "binary frame round-tripped through the relay");
    let _ = client.close(None).await;
}

/// When the upstream runtime WS is unreachable, `relay_to_upstream` fails the
/// connect and closes the client socket (1011) instead of hanging — the same
/// graceful-close path `relay` uses in production.
#[tokio::test]
async fn relay_closes_client_when_upstream_unreachable() {
    if !gated() {
        eprintln!("skipped: set OWUI_WS_LIVE=1 to run the WS relay test");
        return;
    }
    // Relay pointed at TCP port 1 on loopback: nothing listens, so the upstream
    // `connect_async` is refused and the relay closes the client with 1011.
    let app = Router::new()
        .route("/term", get(relay_handler))
        .with_state("ws://127.0.0.1:1/api/terminals/x".to_string());
    let relay_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay listener");
    let relay_addr = relay_listener.local_addr().expect("relay local addr");
    tokio::spawn(async move {
        let _ = axum::serve(relay_listener, app).await;
    });

    let url = format!("ws://{relay_addr}/term");
    let (mut client, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect to relay");

    // The client stream must END (None / error / Close) within the window —
    // proving the relay closed the socket instead of leaving it open.
    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match client.next().await {
                None => return,
                Some(Err(_)) => return,
                Some(Ok(TungMsg::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "client socket ended after the upstream connect failed (no hang)"
    );
}
