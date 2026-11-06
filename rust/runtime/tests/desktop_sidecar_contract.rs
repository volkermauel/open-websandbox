//! Desktop/browser sidecar lane contract tests (HTTP fallback, WS relays,
//! `/api/config` merge).
//!
//! HTTP paths run fully in-process (`tower::oneshot` against the real router,
//! real axum sidecar mock on loopback). The WS relay tests bind real loopback
//! listeners (client + runtime server + WS echo sidecar), so — like the
//! broker's `ws_relay.rs` — they are **env-gated** by `OWUI_WS_LIVE=1`:
//!
//! ```text
//! OWUI_WS_LIVE=1 cargo test -p runtime --test desktop_sidecar_contract -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::net::TcpListener as StdTcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;

use common::{body_text, Env};
use futures_util::{SinkExt, StreamExt};
use runtime::{build_router, AppState, RuntimeConfig, SessionKeyStore};

mod common;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::Message as TungMsg;

/// The shared sidecar Bearer every test configures (`DESKTOP_API_KEY`).
const SIDECAR_KEY: &str = "sidecar-key-123";
/// Body the mock sidecar's fallback answers with (distinct from any runtime
/// body, so a hit is unambiguous).
const SIDECAR_FALLBACK_BODY: &str = "sidecar-fallback-body";
/// The runtime's own flags (app.rs contract) — asserted verbatim below.
const RUNTIME_FLAGS_JSON: &str =
    r#"{"features":{"terminal":true,"notebooks":false,"system":true}}"#;

fn gated() -> bool {
    std::env::var("OWUI_WS_LIVE").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Free-loopback-port helper for the "sidecar down" cases: bind + drop, the
/// port is refused (no listener) immediately after.
fn refused_port() -> u16 {
    let l = StdTcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}

// ---------------------------------------------------------------------------
// Mock sidecar: one real axum server capturing every request it sees.
// ---------------------------------------------------------------------------

/// One request the mock sidecar received.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path_and_query: String,
    authorization: Option<String>,
    content_type: Option<String>,
    x_custom: Option<String>,
    host: Option<String>,
    body: String,
}

#[derive(Clone)]
struct MockSidecar {
    url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
    config_fetches: Arc<Mutex<usize>>,
    fallback_status: Arc<Mutex<StatusCode>>,
}

impl MockSidecar {
    /// Start the mock on an ephemeral loopback port.
    fn start(config_body: serde_json::Value, fallback_status: StatusCode) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let config_fetches = Arc::new(Mutex::new(0));
        let config_body = Arc::new(Mutex::new(config_body));
        let fallback_status = Arc::new(Mutex::new(fallback_status));

        let state = Self {
            url: String::new(),
            captured: captured.clone(),
            config_fetches: config_fetches.clone(),
            fallback_status: fallback_status.clone(),
        };

        let app = Router::new()
            .route(
                "/api/config",
                get({
                    let captured = captured.clone();
                    let config_fetches = config_fetches.clone();
                    let config_body = config_body.clone();
                    move |headers: HeaderMap| async move {
                        *config_fetches.lock().expect("config fetch lock") += 1;
                        captured.lock().expect("capture lock").push(Captured {
                            method: "GET".to_owned(),
                            path_and_query: "/api/config".to_owned(),
                            authorization: headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned),
                            content_type: None,
                            x_custom: None,
                            host: None,
                            body: String::new(),
                        });
                        axum::Json(config_body.lock().expect("config body lock").clone())
                    }
                }),
            )
            .fallback(capture_fallback)
            .with_state(state.clone());

        let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock sidecar");
        std_listener
            .set_nonblocking(true)
            .expect("nonblocking mock sidecar");
        let listener = TcpListener::from_std(std_listener).expect("mock sidecar listener");
        let port = listener.local_addr().expect("mock addr").port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock sidecar serve");
        });

        Self {
            url: format!("http://127.0.0.1:{port}"),
            captured,
            config_fetches,
            fallback_status,
        }
    }

    /// Everything captured so far.
    fn requests(&self) -> Vec<Captured> {
        self.captured.lock().expect("capture lock").clone()
    }

    fn config_fetch_count(&self) -> usize {
        *self.config_fetches.lock().expect("config fetch lock")
    }
}

/// Fallback: capture the request, answer with the configured status + body.
async fn capture_fallback(State(mock): State<MockSidecar>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("mock body");
    mock.captured.lock().expect("capture lock").push(Captured {
        method: parts.method.as_str().to_owned(),
        path_and_query: parts
            .uri
            .path_and_query()
            .map_or_else(|| "/".to_owned(), |pq| pq.as_str().to_owned()),
        authorization: parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        content_type: parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        x_custom: parts
            .headers
            .get("x-custom")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        host: parts
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    });
    let status = *mock.fallback_status.lock().expect("status lock");
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        SIDECAR_FALLBACK_BODY,
    )
        .into_response()
}

/// Env with the desktop lane pointed at `mock` (+ shared bearer).
fn desktop_env(mock: &MockSidecar) -> Env {
    Env::with_config(|cfg| {
        cfg.desktop_url = Some(mock.url.clone());
        cfg.desktop_api_key = Some(SIDECAR_KEY.to_string());
    })
}

// ---------------------------------------------------------------------------
// HTTP fallback (in-process)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fallback_forwards_path_query_body_and_replaces_auth() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = desktop_env(&mock);

    let resp = env
        .send_with(
            Method::POST,
            "/whatever/path?x=1&q=hello%20world",
            &[
                ("x-custom", "keep-me"),
                ("content-type", "application/json"),
            ],
            Some(br#"{"task":"run"}"#.to_vec()),
        )
        .await;

    // Response relayed from the sidecar.
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, SIDECAR_FALLBACK_BODY);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one sidecar hit: {reqs:?}");
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.path_and_query, "/whatever/path?x=1&q=hello%20world");
    assert_eq!(
        r.authorization.as_deref(),
        Some("Bearer sidecar-key-123"),
        "inbound runtime Bearer replaced by the sidecar Bearer"
    );
    assert_eq!(
        r.x_custom.as_deref(),
        Some("keep-me"),
        "custom headers pass"
    );
    assert_eq!(
        r.content_type.as_deref(),
        Some("application/json"),
        "content-type passes through"
    );
    assert!(
        r.host
            .as_deref()
            .is_some_and(|h| h.starts_with("127.0.0.1:")),
        "host is the sidecar authority (inbound host never leaked)"
    );
    assert_eq!(r.body, r#"{"task":"run"}"#);
}

#[tokio::test]
async fn fallback_relays_upstream_status_content_type_and_body() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::CREATED);
    let env = desktop_env(&mock);

    let resp = env
        .send_raw(
            Method::GET,
            "/assets/app.css",
            common::Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain"),
        "sidecar content-type relayed"
    );
    assert_eq!(body_text(resp).await, SIDECAR_FALLBACK_BODY);
    // Authenticated caller: the inbound runtime Bearer is REPLACED by the
    // sidecar Bearer (the runtime key never leaks to the sidecar).
    let r = &mock.requests()[0];
    assert_eq!(r.authorization.as_deref(), Some("Bearer sidecar-key-123"));
}

#[tokio::test]
async fn unmatched_path_is_default_404_when_desktop_unconfigured() {
    let env = Env::new();
    let resp = env
        .send(
            Method::GET,
            "/definitely/not/here",
            common::Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Pre-desktop behaviour: axum's default empty 404 body.
    assert!(
        body_bytes(resp).await.is_empty(),
        "404 body must stay empty"
    );
}

#[tokio::test]
async fn desktop_enabled_false_is_a_hard_kill_switch() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = Env::with_config(|cfg| {
        cfg.desktop_url = Some(mock.url.clone());
        cfg.desktop_api_key = Some(SIDECAR_KEY.to_string());
        cfg.desktop_enabled = false;
    });

    let resp = env
        .send(
            Method::GET,
            "/whatever",
            common::Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(mock.requests().is_empty(), "sidecar must never be hit");
}

#[tokio::test]
async fn non_upgrade_or_non_get_request_to_a_ws_route_forwards_like_fallback() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = desktop_env(&mock);

    // POST (no upgrade) on an explicit WS path: must forward, never 405.
    let resp = env
        .send_with(
            Method::POST,
            "/events",
            &[("content-type", "text/plain")],
            Some(b"payload".to_vec()),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let r = &mock.requests()[0];
    assert_eq!(r.method, "POST");
    assert_eq!(r.path_and_query, "/events");
    assert_eq!(r.body, "payload");
}

#[tokio::test]
async fn explicit_routes_win_over_the_sidecar() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = desktop_env(&mock);

    // Open explicit route: served by the runtime, not the sidecar. (`ok()`
    // answers an EMPTY 200 body — the pre-desktop probe contract.)
    let health = env
        .send(Method::GET, "/healthz", common::Bearer::None, None, None)
        .await;
    assert_eq!(health.status(), StatusCode::OK);
    assert!(body_bytes(health).await.is_empty());

    // Gated explicit route: broker Bearer still enforced even with the lane on.
    let unauth = env
        .send(Method::POST, "/execute", common::Bearer::None, None, None)
        .await;
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    assert!(
        mock.requests().is_empty(),
        "explicit routes must never reach the sidecar"
    );
}

// ---------------------------------------------------------------------------
// GET /api/config — merge
// ---------------------------------------------------------------------------

fn sidecar_config_json() -> serde_json::Value {
    serde_json::json!({
        "features": {
            "terminal": true,
            "notebooks": true,
            "desktop": true,
            "browser": true,
            "git": true,
            "events": true,
        },
        "auth": {"type": "none"},
    })
}

#[tokio::test]
async fn api_config_merges_sidecar_flags_runtime_flags_win() {
    let mock = MockSidecar::start(sidecar_config_json(), StatusCode::OK);
    let env = desktop_env(&mock);

    let resp = env
        .send(Method::GET, "/api/config", common::Bearer::None, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let merged: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("valid json");

    let features = &merged["features"];
    // Runtime's own flags kept verbatim — the sidecar must NOT flip them.
    assert_eq!(features["terminal"], true);
    assert_eq!(features["notebooks"], false, "runtime flag wins conflicts");
    assert_eq!(features["system"], true);
    // Sidecar-only flags added.
    assert_eq!(features["desktop"], true);
    assert_eq!(features["browser"], true);
    assert_eq!(features["git"], true);
    assert_eq!(features["events"], true);
    // Sidecar top-level keys added.
    assert_eq!(merged["auth"]["type"], "none");
    // The sidecar fetch authenticated with the shared bearer.
    assert_eq!(
        mock.requests()[0].authorization.as_deref(),
        Some("Bearer sidecar-key-123")
    );
}

#[tokio::test]
async fn api_config_serves_runtime_flags_when_sidecar_is_down() {
    let dead = format!("http://127.0.0.1:{}", refused_port());
    let env = Env::with_config(|cfg| {
        cfg.desktop_url = Some(dead);
        cfg.desktop_api_key = Some(SIDECAR_KEY.to_string());
    });

    let resp = env
        .send(Method::GET, "/api/config", common::Bearer::None, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "sidecar down must never 500");
    let merged: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("valid json");
    assert_eq!(
        merged,
        serde_json::from_str::<serde_json::Value>(RUNTIME_FLAGS_JSON).unwrap(),
        "exactly the runtime's own flags"
    );
}

#[tokio::test]
async fn api_config_serves_runtime_flags_when_lane_inactive() {
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/api/config", common::Bearer::None, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let merged: serde_json::Value =
        serde_json::from_str(&body_text(resp).await).expect("valid json");
    assert_eq!(
        merged,
        serde_json::from_str::<serde_json::Value>(RUNTIME_FLAGS_JSON).unwrap()
    );
}

#[tokio::test]
async fn api_config_fetches_are_cached_for_a_few_seconds() {
    let mock = MockSidecar::start(sidecar_config_json(), StatusCode::OK);
    let env = desktop_env(&mock);

    for _ in 0..3 {
        let resp = env
            .send(Method::GET, "/api/config", common::Bearer::None, None, None)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_text(resp).await.contains("\"desktop\":true"));
    }
    assert_eq!(
        mock.config_fetch_count(),
        1,
        "burst within the TTL must fetch once"
    );
}

// ---------------------------------------------------------------------------
// Per-session runtime-key auth on the desktop lane (same guard as every other
// runtime route; only /api/config stays open)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fallback_requires_the_runtime_key() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = desktop_env(&mock);

    for (label, bearer) in [
        ("no key", common::Bearer::None),
        ("wrong key", common::Bearer::Explicit("wrong-key")),
    ] {
        let resp = env
            .send_raw(Method::GET, "/whatever/path", bearer, None, None)
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{label} must be rejected"
        );
    }
    assert!(
        mock.requests().is_empty(),
        "rejected requests must never reach the sidecar"
    );

    let ok = env
        .send_raw(
            Method::GET,
            "/whatever/path",
            common::Bearer::Default,
            None,
            None,
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK, "the right key forwards");
    assert_eq!(body_text(ok).await, SIDECAR_FALLBACK_BODY);
}

#[tokio::test]
async fn ws_routes_require_the_runtime_key_for_plain_requests() {
    let mock = MockSidecar::start(serde_json::json!({}), StatusCode::OK);
    let env = desktop_env(&mock);

    for path in [
        "/desktop/stream",
        "/desktop/audio",
        "/events",
        "/browser/view/s-1/tok/ws",
    ] {
        for (label, bearer) in [
            ("no key", common::Bearer::None),
            ("wrong key", common::Bearer::Explicit("wrong-key")),
        ] {
            let resp = env.send_raw(Method::GET, path, bearer, None, None).await;
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{path} with {label} must be rejected"
            );
        }
    }
    assert!(
        mock.requests().is_empty(),
        "rejected requests must never reach the sidecar"
    );

    let ok = env
        .send_raw(Method::GET, "/events", common::Bearer::Default, None, None)
        .await;
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "the right key keeps the fallback semantics"
    );
}

#[tokio::test]
async fn api_config_stays_open_without_a_key() {
    let mock = MockSidecar::start(sidecar_config_json(), StatusCode::OK);
    let env = desktop_env(&mock);
    let resp = env
        .send(Method::GET, "/api/config", common::Bearer::None, None, None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/api/config is feature discovery: read before any session exists"
    );
    assert!(body_text(resp).await.contains("\"desktop\":true"));
}

#[tokio::test]
async fn inactive_lane_fallback_is_fail_closed_without_a_key() {
    // Lane off: a VALID key keeps the pre-desktop empty 404, but an absent key
    // is a 401 — the auth guard runs before the lane check, so the fallback is
    // never an unauthenticated probe surface.
    let env = Env::new();
    let resp = env
        .send(Method::GET, "/whatever", common::Bearer::None, None, None)
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// WS relays (env-gated: real loopback listeners)
// ---------------------------------------------------------------------------

type SeenAuth = Arc<Mutex<Vec<String>>>;
type SeenClose = Arc<Mutex<Vec<u16>>>;

/// Minimal WS echo server standing in for the desktop sidecar: records the
/// upgrade's Authorization header and every close-frame code, echoes all
/// other frames verbatim (text AND binary — both directions must survive).
async fn ws_echo_server(listener: TcpListener, seen_auth: SeenAuth, seen_close: SeenClose) {
    while let Ok((stream, _)) = listener.accept().await {
        let seen_auth = seen_auth.clone();
        let seen_close = seen_close.clone();
        tokio::spawn(async move {
            #[allow(clippy::result_large_err)] // tungstenite's fixed callback error (~136 B)
            let callback =
                |req: &WsRequest, resp: WsResponse| -> Result<WsResponse, ErrorResponse> {
                    if let Some(a) = req.headers().get("authorization") {
                        seen_auth
                            .lock()
                            .expect("auth lock")
                            .push(a.to_str().unwrap_or_default().to_owned());
                    }
                    Ok(resp)
                };
            let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                return;
            };
            let (mut sink, mut stream) = ws.split();
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    TungMsg::Close(frame) => {
                        seen_close
                            .lock()
                            .expect("close lock")
                            .push(frame.map_or(1005, |f| f.code.into()));
                        break;
                    }
                    m => {
                        if sink.send(m).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

/// Build a full runtime server on an ephemeral port with the desktop lane
/// pointed at `sidecar_url`; returns `(ws://127.0.0.1:<port>, tempdir)`. The
/// caller keeps the `TempDir` alive so the projected key file outlives every
/// request.
async fn spawn_runtime_server(sidecar_url: String) -> (String, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tmp dir");
    let key_path = tmp.path().join("api-key");
    std::fs::write(&key_path, common::STRONG_KEY).expect("write key");
    let config = RuntimeConfig {
        workdir: tmp.path().join("workspace"),
        runtime_key_file: key_path.clone(),
        desktop_url: Some(sidecar_url),
        desktop_api_key: Some(SIDECAR_KEY.to_string()),
        ..RuntimeConfig::default()
    };
    // The store reads the PROJECTED KEY FILE (same contract as production) —
    // not the value; a store pointed at a non-existent path would fail closed
    // with 503 on the first Authed extraction.
    let state = AppState::new(config, SessionKeyStore::new(&key_path));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind runtime");
    let port = listener.local_addr().expect("runtime addr").port();
    tokio::spawn(async move {
        axum::serve(listener, build_router(state))
            .await
            .expect("runtime serve");
    });
    (format!("ws://127.0.0.1:{port}"), tmp)
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect a WS client to `url`, optionally authenticating with the per-session
/// runtime key (the desktop lane requires it, like every gated runtime route).
async fn connect_ws(
    url: &str,
    key: Option<&str>,
) -> Result<WsStream, tokio_tungstenite::tungstenite::Error> {
    let mut req = url.into_client_request().expect("client request");
    if let Some(k) = key {
        req.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {k}")).expect("header value"),
        );
    }
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
}

#[tokio::test]
async fn ws_upgrade_requires_the_runtime_key() {
    if !gated() {
        return;
    }
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let echo_port = echo.local_addr().expect("echo addr").port();
    tokio::spawn(ws_echo_server(
        echo,
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let (runtime_ws, _tmp) = spawn_runtime_server(format!("http://127.0.0.1:{echo_port}")).await;

    // Missing / wrong key: the upgrade is refused with 401, pre-upgrade.
    for key in [None, Some("wrong-key")] {
        let err = connect_ws(&format!("{runtime_ws}/events"), key)
            .await
            .expect_err("unauthenticated upgrade must be refused");
        match err {
            tokio_tungstenite::tungstenite::error::Error::Http(resp) => {
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "key {key:?}");
            }
            other => panic!("expected an HTTP 401 rejection, got {other:?}"),
        }
    }

    // The correct key upgrades and relays.
    let socket = connect_ws(&format!("{runtime_ws}/events"), Some(common::STRONG_KEY))
        .await
        .expect("correct key upgrades");
    let (mut sink, mut stream) = socket.split();
    expect_echo(
        &mut sink,
        &mut stream,
        TungMsg::Text("hello-auth".into()),
        "authed",
    )
    .await;
}

/// Send `frame` and expect the exact same frame back within 5s.
async fn expect_echo(
    sink: &mut futures_util::stream::SplitSink<WsStream, TungMsg>,
    stream: &mut futures_util::stream::SplitStream<WsStream>,
    frame: TungMsg,
    label: &str,
) {
    let want = frame.clone();
    sink.send(frame).await.expect("send frame");
    let got = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("echo within 5s")
        .expect("stream open")
        .expect("no ws error");
    assert_eq!(got, want, "{label} frame must round-trip verbatim");
}

#[tokio::test]
async fn ws_relay_events_relays_frames_and_authenticates_upstream() {
    if !gated() {
        return;
    }
    let seen_auth: SeenAuth = Arc::new(Mutex::new(Vec::new()));
    let seen_close: SeenClose = Arc::new(Mutex::new(Vec::new()));
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let echo_port = echo.local_addr().expect("echo addr").port();
    tokio::spawn(ws_echo_server(echo, seen_auth.clone(), seen_close));

    let (runtime_ws, _tmp) = spawn_runtime_server(format!("http://127.0.0.1:{echo_port}")).await;

    let socket = connect_ws(
        &format!("{runtime_ws}/events?client=1"),
        Some(common::STRONG_KEY),
    )
    .await
    .expect("connect runtime ws");
    let (mut sink, mut stream) = socket.split();

    expect_echo(
        &mut sink,
        &mut stream,
        TungMsg::Text("hello-relay".into()),
        "text",
    )
    .await;
    expect_echo(
        &mut sink,
        &mut stream,
        TungMsg::Binary(vec![9u8, 8, 7].into()),
        "binary",
    )
    .await;

    // The upstream hop carried the sidecar Bearer.
    let auths = seen_auth.lock().expect("auth lock");
    assert_eq!(
        auths.last().map(String::as_str),
        Some("Bearer sidecar-key-123"),
        "WS upgrade must authenticate with the shared sidecar bearer"
    );
}

#[tokio::test]
async fn ws_relay_browser_view_path_relays_frames() {
    if !gated() {
        return;
    }
    let seen_auth: SeenAuth = Arc::new(Mutex::new(Vec::new()));
    let seen_close: SeenClose = Arc::new(Mutex::new(Vec::new()));
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let echo_port = echo.local_addr().expect("echo addr").port();
    tokio::spawn(ws_echo_server(echo, seen_auth.clone(), seen_close));

    let (runtime_ws, _tmp) = spawn_runtime_server(format!("http://127.0.0.1:{echo_port}")).await;

    let socket = connect_ws(
        &format!("{runtime_ws}/browser/view/sid-1/tok/ws"),
        Some(common::STRONG_KEY),
    )
    .await
    .expect("connect runtime ws");
    let (mut sink, mut stream) = socket.split();

    expect_echo(
        &mut sink,
        &mut stream,
        TungMsg::Binary(vec![42u8].into()),
        "browser",
    )
    .await;
    assert_eq!(
        seen_auth
            .lock()
            .expect("auth lock")
            .last()
            .map(String::as_str),
        Some("Bearer sidecar-key-123")
    );
}

#[tokio::test]
async fn ws_relay_forwards_client_close_frame_upstream() {
    if !gated() {
        return;
    }
    let seen_auth: SeenAuth = Arc::new(Mutex::new(Vec::new()));
    let seen_close: SeenClose = Arc::new(Mutex::new(Vec::new()));
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let echo_port = echo.local_addr().expect("echo addr").port();
    tokio::spawn(ws_echo_server(echo, seen_auth.clone(), seen_close.clone()));

    let (runtime_ws, _tmp) = spawn_runtime_server(format!("http://127.0.0.1:{echo_port}")).await;

    let socket = connect_ws(
        &format!("{runtime_ws}/desktop/stream"),
        Some(common::STRONG_KEY),
    )
    .await
    .expect("connect runtime ws");
    let (mut sink, _stream) = socket.split();

    sink.send(TungMsg::Close(Some(
        tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Iana(3001),
            reason: "client-done".into(),
        },
    )))
    .await
    .expect("send close");

    // The echo sidecar must have SEEN the client's close code (verbatim
    // close-frame propagation) within a few seconds.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen_close.lock().expect("close lock").contains(&3001) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("close code 3001 must reach the sidecar");
}

#[tokio::test]
async fn ws_relay_sidecar_down_closes_with_1011() {
    if !gated() {
        return;
    }
    let dead = format!("http://127.0.0.1:{}", refused_port());
    let (runtime_ws, _tmp) = spawn_runtime_server(dead).await;

    let mut ws = connect_ws(&format!("{runtime_ws}/events"), Some(common::STRONG_KEY))
        .await
        .expect("runtime accepts the upgrade");

    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("close within 5s")
        .expect("stream open")
        .expect("no ws error");
    match msg {
        TungMsg::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 1011, "sidecar down ⇒ close 1011");
        }
        other => panic!("expected close 1011, got {other:?}"),
    }
}

/// Read a response body to raw bytes (local re-implementation to keep the
/// common harness surface untouched by this file's needs).
async fn body_bytes(resp: Response<Body>) -> Bytes {
    resp.into_body()
        .collect()
        .await
        .expect("body collect")
        .to_bytes()
}
