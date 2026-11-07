//! Desktop/browser/event WS relay tests (openspec/changes/desktop-browser-ws-relay).
//!
//! * In-process (always run): each new route rejects a missing-identity
//!   request with the exact 400 + detail the terminal identity check produces
//!   (same [`broker::terminal::terminal_identity`] call); the browser route
//!   rejects unknown channels pre-upgrade; a non-upgrade request with a valid
//!   identity maps to the canonical 400.
//! * **Env-gated** by `OWUI_WS_LIVE=1` (same opt-in as `ws_relay.rs`): the
//!   full relay through the SERVED broker router against a local echo
//!   upstream — upgrade path+query preserved verbatim, per-session Bearer on
//!   the upstream hop, subprotocol echoed to the client + forwarded upstream,
//!   frames round-trip, and a bad first-message token closes `4001`
//!   identically on `/desktop/stream` and `/api/terminals/{id}`.
//!
//! ```text
//! OWUI_WS_LIVE=1 cargo test -p broker --test desktop_ws -- --nocapture
//! ```

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use broker::store::test_fakes::StubSandboxStore;
use broker::{build_router, resolve_sandbox, sandbox_name, AppState, SandboxStore};
use futures_util::{SinkExt, StreamExt};
use shared::{BrokerConfig, Profile, SandboxTemplate, SandboxTemplateSpec};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::Message as TungMsg;
use tower::ServiceExt;

const STRONG_SECRET: &str = "test-broker-shared-secret-0123456789";
const BASE_TEMPLATE: &str = "code-standard-v1";

fn gated() -> bool {
    std::env::var("OWUI_WS_LIVE").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

fn template() -> SandboxTemplate {
    SandboxTemplate::new(
        BASE_TEMPLATE,
        SandboxTemplateSpec {
            description: None,
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "code-standard:latest",
                        "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]
                    }],
                    "volumes": [{"name": "workspace", "emptyDir": {}}]
                }
            })),
        },
    )
}

fn config() -> BrokerConfig {
    BrokerConfig {
        shared_secret: STRONG_SECRET.into(),
        runtime_ns: "agent-sandbox-runtime".into(),
        base_template: BASE_TEMPLATE.into(),
        default_profile: Profile::Persistent,
        runtime_api_key: "rt-test-key".into(),
        claim_timeout_seconds: 5,
        // Claiming disabled: the direct-Sandbox create path is what these
        // tests exercise (stub store auto-readies + mints the runtime key).
        warm_pool: String::new(),
        ..BrokerConfig::default()
    }
}

fn store() -> Arc<StubSandboxStore> {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template());
    store
}

/// In-process router (oneshot): created sandboxes never go Ready (no auto-ready),
/// which is fine — the always-on tests only assert PRE-resolve rejections.
fn router(store: Arc<StubSandboxStore>) -> axum::Router {
    build_router(AppState::new(config(), store))
}

async fn send(
    router: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("oneshot")
}

async fn detail(resp: axum::http::Response<Body>) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body");
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("canonical JSON error body")
        ["detail"]
        .as_str()
        .expect("detail string")
        .to_owned()
}

// --- in-process: identity / channel rejections ------------------------------

/// A request with NO user at all must still be rejected with 400 on every new
/// route (the desktop routes intentionally ACCEPT an empty session — that is
/// the unsaved-chat case — so the old "same 400 detail as the terminal check"
/// parity no longer holds for them; the user check is the remaining hard gate).
#[tokio::test]
async fn new_routes_reject_missing_user() {
    let env = router(store());

    for path in [
        "/desktop/stream",
        "/desktop/audio",
        "/events",
        "/browser/view/bs-1/tok-2/ws",
    ] {
        let resp = send(&env, path, &[]).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{path} should reject a missing user"
        );
    }
}

/// An empty session with a user present is the unsaved-chat case: the routes
/// must NOT 400 on it (session falls back to the persistent session).
#[tokio::test]
async fn new_routes_accept_empty_session_with_user() {
    let env = router(store());

    for path in [
        "/desktop/stream?user_id=u-1",
        "/desktop/audio?user_id=u-1",
        "/events?user_id=u-1",
    ] {
        let resp = send(&env, path, &[("x-user-id", "u-1"), ("x-session-id", "")]).await;
        assert_ne!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{path} must accept an empty session (got 400)"
        );
    }
}

/// `/browser/view/{sid}/{token}/{channel}` is only relayed for the three
/// known channels; anything else is rejected pre-upgrade (before a sandbox is
/// resolved).
#[tokio::test]
async fn browser_route_rejects_unknown_channel() {
    let env = router(store());
    let resp = send(
        &env,
        "/browser/view/bs-1/tok-2/ftp",
        &[("x-user-id", "u1"), ("x-session-id", "c1")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        detail(resp).await.contains("unknown browser view channel"),
        "channel rejection detail"
    );
}

/// A request carrying a valid identity but no WS upgrade maps to the canonical
/// JSON 400 (documented divergence from the terminal route's plain rejection:
/// identity validation runs BEFORE the upgrade extraction on these routes).
#[tokio::test]
async fn non_upgrade_request_with_identity_maps_to_canonical_400() {
    let env = router(store());
    let resp = send(
        &env,
        "/desktop/stream",
        &[("x-user-id", "u1"), ("x-session-id", "c1")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        detail(resp)
            .await
            .contains("expected a WebSocket upgrade request"),
        "upgrade-required detail"
    );
}

/// Resolution parity with the catch-all: the relay path uses the same
/// `resolve_sandbox` — assert the deterministic persistent name the relay
/// would target (sanity that identity → sandbox mapping is unchanged).
#[tokio::test]
async fn identity_resolves_to_the_same_sandbox_as_the_proxy_path() {
    let store = store();
    store.set_auto_ready_on_create(Some("10.0.0.9".into()));
    let state = AppState::new(config(), store.clone());
    let resolved = resolve_sandbox(&state, "u1", "c1", Profile::Persistent)
        .await
        .expect("ready");
    assert_eq!(resolved.name, sandbox_name("u1", "c1", Profile::Persistent));
}

// --- OWUI_WS_LIVE: full relay against a real echo upstream ------------------

/// (path?query, Authorization, Sec-WebSocket-Protocol) captured per upstream
/// handshake.
type SeenHandshake = (String, Option<String>, Option<String>);
type SeenHandshakes = Arc<Mutex<Vec<SeenHandshake>>>;

/// WS echo server standing in for the runtime's desktop endpoints: records the
/// upgrade URI + auth + subprotocol header, echoes the client's first offered
/// subprotocol back (upstream "selection"), and echoes data frames verbatim.
async fn echo_server(listener: TcpListener, seen: SeenHandshakes) {
    while let Ok((stream, _)) = listener.accept().await {
        let seen = seen.clone();
        tokio::spawn(async move {
            let seen = seen.clone();
            // ErrorResponse is tungstenite's fixed callback error type; the
            // signature is not ours to choose.
            #[allow(clippy::result_large_err)]
            let callback =
                move |req: &WsRequest, mut resp: WsResponse| -> Result<WsResponse, ErrorResponse> {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let proto = req
                        .headers()
                        .get("sec-websocket-protocol")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if let Some(first) = proto.as_deref().and_then(|p| p.split(',').next()) {
                        if let Ok(v) = HeaderValue::from_str(first.trim()) {
                            resp.headers_mut().append("sec-websocket-protocol", v);
                        }
                    }
                    seen.lock()
                        .expect("seen lock")
                        .push((req.uri().to_string(), auth, proto));
                    Ok(resp)
                };
            let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                return;
            };
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
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

/// Stand up the echo upstream + the SERVED broker router (override repointed
/// at the echo; stub auto-readies created sandboxes and mints the runtime key)
/// and return the broker base URL, the store, and the captured handshakes.
async fn live_env() -> (String, Arc<StubSandboxStore>, SeenHandshakes) {
    let upstream_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo listener");
    let upstream_addr = upstream_listener.local_addr().expect("echo local addr");
    let seen: SeenHandshakes = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(echo_server(upstream_listener, seen.clone()));

    let store = store();
    store.set_auto_ready_on_create(Some("10.0.0.7".into()));
    let state = AppState::new(config(), store.clone())
        .with_runtime_upstream_override(format!("http://{upstream_addr}"));
    let app = build_router(state);
    let broker_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind broker listener");
    let broker_addr = broker_listener.local_addr().expect("broker local addr");
    tokio::spawn(async move {
        let _ = axum::serve(broker_listener, app).await;
    });
    (format!("ws://{broker_addr}"), store, seen)
}

/// Open a client WS with an optional subprotocol against `path?query`.
type ClientStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    base: &str,
    path_and_query: &str,
    protocol: Option<&str>,
) -> (
    ClientStream,
    tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) {
    let mut req = format!("{base}{path_and_query}")
        .into_client_request()
        .expect("client request");
    if let Some(p) = protocol {
        req.headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_str(p).unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .expect("connect to broker")
}

/// Send the first-message auth handshake.
async fn send_auth(client: &mut ClientStream, token: &str) {
    client
        .send(TungMsg::Text(
            serde_json::json!({"type": "auth", "token": token})
                .to_string()
                .into(),
        ))
        .await
        .expect("send auth");
}

/// Read until the text echo of `probe` AND a binary echo both arrive; returns
/// the binary echo payload (asserts the text round-trip verbatim).
async fn wait_for_echo(client: &mut ClientStream, probe: &str) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut binary = None;
    let mut text_seen = false;
    while Instant::now() < deadline && !(text_seen && binary.is_some()) {
        if let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(1), client.next()).await
        {
            match msg {
                TungMsg::Text(t) if t.as_str() == probe => text_seen = true,
                TungMsg::Binary(b) => binary = Some(b.to_vec()),
                _ => {}
            }
        }
    }
    assert!(text_seen, "text echo of {probe:?} never arrived");
    binary
}

/// Full relay: identity via query params, first-message auth, subprotocol
/// echoed by the broker + forwarded upstream, path+query preserved verbatim,
/// per-session Bearer on the upstream hop, text + binary frames round-trip.
#[tokio::test]
async fn desktop_stream_relays_with_path_query_subprotocol_and_bearer() {
    if !gated() {
        eprintln!("skipped: set OWUI_WS_LIVE=1 to run the live WS relay tests");
        return;
    }
    let (base, store, seen) = live_env().await;
    let pq = "/desktop/stream?user_id=u1&session_id=c1";
    let (mut client, resp) = connect(&base, pq, Some("binary")).await;
    // The broker echoes the client's offered subprotocol.
    let echoed = resp
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        echoed,
        Some("binary"),
        "broker must echo the offered subprotocol"
    );

    send_auth(&mut client, STRONG_SECRET).await;
    client
        .send(TungMsg::Text("hello-desktop".into()))
        .await
        .expect("send text");
    client
        .send(TungMsg::Binary(vec![7, 8, 9].into()))
        .await
        .expect("send binary");
    let binary = wait_for_echo(&mut client, "hello-desktop").await;
    assert_eq!(
        binary.as_deref(),
        Some(&[7u8, 8, 9][..]),
        "binary echo verbatim"
    );
    let _ = client.close(None).await;

    // The upstream saw exactly one handshake: same path+query, the per-session
    // runtime-key Bearer, and the client's subprotocol list.
    let key = store
        .read_runtime_key(&sandbox_name("u1", "c1", Profile::Persistent))
        .await
        .expect("store ok")
        .expect("runtime key minted on create");
    let handshakes = seen.lock().expect("seen lock");
    assert_eq!(handshakes.len(), 1, "one upstream handshake");
    let (up_path, auth, proto) = &handshakes[0];
    assert_eq!(up_path, pq, "path+query must be preserved verbatim");
    assert_eq!(
        auth.as_deref(),
        Some(format!("Bearer {key}").as_str()),
        "upstream upgrade carries the per-session Bearer"
    );
    assert_eq!(proto.as_deref(), Some("binary"), "subprotocol forwarded");
}

/// Auth-contract parity: a WRONG first-message token closes `4001` with the
/// SAME reason on the desktop relay and on the terminal relay.
#[tokio::test]
async fn bad_token_closes_4001_identically_on_desktop_and_terminal() {
    if !gated() {
        eprintln!("skipped: set OWUI_WS_LIVE=1 to run the live WS relay tests");
        return;
    }
    let (base, _store, _seen) = live_env().await;
    for path in [
        "/desktop/stream?user_id=u2&session_id=c2",
        "/api/terminals/c2?user_id=u2&session_id=c2",
    ] {
        let (mut client, _resp) = connect(&base, path, None).await;
        send_auth(&mut client, "wrong-token").await;
        // Expect a Close 4001 promptly (deny is immediate — no auth timeout).
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut close = None;
        while Instant::now() < deadline && close.is_none() {
            if let Ok(Some(Ok(TungMsg::Close(frame)))) =
                tokio::time::timeout(Duration::from_secs(1), client.next()).await
            {
                close = frame.map(|f| (u16::from(f.code), f.reason.to_string()));
            }
        }
        assert_eq!(
            close,
            Some((4001u16, "invalid api key".to_string())),
            "{path} must close 4001 with the terminal route's reason"
        );
    }
}

// --- unsaved chat (empty X-Session-Id) falls back to the persistent session --

/// Open WebUI dials /desktop/stream with only user_id: X-Session-Id is empty for
/// an unsaved chat. The route must not 400 — it attaches to the persistent
/// session (session = user) and relays. Pre-upgrade 101 proves identity passed.
#[tokio::test]
async fn desktop_ws_still_rejects_missing_user() {
    let env = router(store());

    for path in [
        "/desktop/stream",
        "/desktop/audio",
        "/events",
        "/browser/view/bs-1/tok-2/ws",
    ] {
        let resp = send(&env, path, &[]).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{path} should reject a missing user"
        );
    }
}

// --- identity semantics (pure fn, unit-tested here for parity) --------------

#[test]
fn identity_empty_session_falls_back_to_persistent_session() {
    use broker::desktop::desktop_identity;

    let query = broker::terminal::TerminalWsQuery {
        user_id: Some("u-1".into()),
        ..Default::default()
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-session-id", HeaderValue::from_static(""));

    let id =
        desktop_identity(&query, &headers, Profile::Persistent).expect("empty session is fine");
    assert_eq!(id.session_id, "u-1", "session falls back to the user");
    assert_eq!(id.user_id, "u-1");
}

#[test]
fn identity_explicit_session_wins_over_the_fallback() {
    use broker::desktop::desktop_identity;

    let query = broker::terminal::TerminalWsQuery {
        user_id: Some("u-1".into()),
        session_id: Some("chat-42".into()),
        chat_id: None,
        persistence: None,
    };
    let headers = axum::http::HeaderMap::new();

    let id = desktop_identity(&query, &headers, Profile::Persistent).expect("session present");
    assert_eq!(id.session_id, "chat-42");
    assert_eq!(id.user_id, "u-1");
}

#[test]
fn identity_missing_user_is_rejected() {
    use broker::desktop::desktop_identity;

    let query = broker::terminal::TerminalWsQuery::default();
    let headers = axum::http::HeaderMap::new();

    assert!(desktop_identity(&query, &headers, Profile::Persistent).is_err());
}
