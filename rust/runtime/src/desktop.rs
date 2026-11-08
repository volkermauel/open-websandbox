//! Desktop/browser sidecar lane (issue: one Open WebUI terminal connection
//! serving terminal AND desktop/browser tabs).
//!
//! A second container in the sandbox pod runs the `open-terminal` desktop
//! image on `:8000`. When `DESKTOP_URL` is set (and `DESKTOP_ENABLED` is not
//! `false`), the runtime surfaces that sidecar on its own `:8888` listener:
//!
//! * **HTTP fallback** — every request that matches no explicit runtime route
//!   is reverse-proxied to the sidecar (method, path, query, body and headers
//!   preserved; hop-by-hop headers stripped; the caller's `Authorization` is
//!   replaced by `Bearer $DESKTOP_API_KEY`). Responses are streamed back.
//!   Unmatched requests are HTTP-only by design — WebSocket clients use the
//!   explicit routes below, so an upgrade never reaches the fallback.
//! * **WS relays** — `/desktop/stream`, `/desktop/audio`, `/events` and
//!   `/browser/view/{sid}/{token}/{channel}` upgrade the client socket and
//!   relay every frame (text + binary + ping/pong, close frames forwarded
//!   with code/reason) to `ws://<DESKTOP_URL><same path?query>`. There are no
//!   idle timers: an idle socket is never killed, and frame size caps match
//!   the configured upload budget (`MAX_UPLOAD_BYTES`), never smaller.
//! * **`GET /api/config` merge** — the runtime's own feature flags stay the
//!   base; the sidecar's flags are merged in for keys the runtime does not
//!   itself define (desktop/browser/git/events appear when the sidecar
//!   reports them). The fetch is bounded by a short timeout, cached for a few
//!   seconds (single-flight), and a down/hung sidecar degrades to the
//!   runtime's own flags — never a 500.
//!
//! With `DESKTOP_URL` unset (or `DESKTOP_ENABLED=false`) every behaviour in
//! this module is inert: unmatched requests keep the pre-desktop default 404
//! and `/api/config` serves the runtime flags unchanged.
//!
//! Auth posture: the sidecar hop is inside the sandbox pod (localhost), so it
//! never sees caller credentials. The desktop/browser-facing SURFACES are
//! broker-relay-only, so — like every other runtime route — they require the
//! per-session runtime key via `Authed`: `/desktop/stream`, `/desktop/audio`,
//! `/events`, `/browser/view/{sid}/{token}/{channel}` and every non-`/api/config`
//! fallback request. The broker's runtime hop already sends
//! `Authorization: Bearer <per-session runtime key>`. Only `GET /api/config`
//! stays unauthenticated (OWUI reads it before any session exists, like
//! upstream's feature discovery).

#![forbid(unsafe_code)]

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message as AxumMsg, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request as AxumRequest, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as TungCloseFrameCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame as TungCloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message as TungMsg;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Hop-by-hop headers never forwarded in either direction (RFC 7230 §6.1 —
/// same set the broker's runtime hop strips; `upgrade`/`connection` included
/// because the fallback is deliberately HTTP-only).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Cached sidecar `GET /api/config` payload + fetch time.
#[derive(Debug, Clone)]
pub struct DesktopConfigCache {
    /// When the payload was fetched (TTL anchor).
    pub fetched_at: Instant,
    /// The sidecar's parsed `/api/config` JSON document.
    pub payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// HTTP fallback
// ---------------------------------------------------------------------------

/// `Router::fallback` — reverse-proxy every UNMATCHED request to the desktop
/// sidecar. Method, path, query, body and headers are preserved; hop-by-hop
/// headers are stripped and the inbound `Authorization` is replaced by the
/// configured sidecar Bearer. When the desktop lane is inactive this returns
/// axum's default empty 404 for an authenticated caller — the pre-desktop
/// behaviour (the `Authed` guard runs first: an unauthenticated caller is a
/// 401 even with the lane off, never an open probe surface).
pub async fn desktop_fallback(
    _: Authed,
    State(state): State<AppState>,
    req: AxumRequest<Body>,
) -> Response {
    if !state.config.desktop_lane_active() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |pq| pq.as_str().to_owned());
    let method = parts.method.clone();
    let headers = parts.headers.clone();
    let bytes = match axum::body::to_bytes(body, state.config.max_upload_bytes as usize).await {
        Ok(b) => b,
        Err(e) => {
            return ApiError::BadRequest(format!("could not read request body: {e}"))
                .into_response()
        }
    };
    match forward_to_sidecar(&state, &path_and_query, method, &headers, bytes).await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

/// Core HTTP forward shared by the fallback and the non-upgrade branch of the
/// WS routes: rebuild headers, inject the sidecar Bearer, send via the
/// no-total-timeout desktop client, stream the response back.
async fn forward_to_sidecar(
    state: &AppState,
    path_and_query: &str,
    method: Method,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == "host" || n == "content-length" || n == "authorization" {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    if let Some(key) = state.config.desktop_api_key.as_deref() {
        match HeaderValue::from_str(&format!("Bearer {key}")) {
            Ok(v) => {
                forwarded.insert(header::AUTHORIZATION, v);
            }
            Err(e) => tracing::warn!("desktop api key is not a valid header value: {e}"),
        }
    }

    let url = format!("{}{}", desktop_base_url(state), path_and_query);
    let mut request = state
        .desktop_client
        .request(method, &url)
        .headers(forwarded);
    if !body.is_empty() {
        request = request.body(body);
    }
    let upstream = request.send().await.map_err(|e| {
        if e.is_timeout() {
            ApiError::GatewayTimeout(format!("timeout forwarding to desktop sidecar: {e}"))
        } else {
            ApiError::BadGateway(format!("desktop sidecar unreachable: {e}"))
        }
    })?;

    // Stream the upstream body straight through (no buffering of large
    // desktop/screenshot/download payloads). `content-length` stays truthful:
    // the desktop client does no decompression, and stripping it would force
    // chunked encoding onto every response.
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let mut builder = Response::builder().status(status);
    for (name, value) in &upstream_headers {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| ApiError::Internal(format!("desktop proxy response build failed: {e}")))
}

/// Sidecar base URL with any trailing slash trimmed (the forwarded path always
/// supplies the leading `/`, so `DESKTOP_URL=http://host:8000/` and
/// `http://host:8000` behave identically and never produce `//`).
fn desktop_base_url(state: &AppState) -> String {
    state
        .config
        .desktop_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_owned()
}

// ---------------------------------------------------------------------------
// WS relay routes
// ---------------------------------------------------------------------------

/// Shared handler for the explicit desktop WS routes: an upgrade request is
/// relayed to the sidecar socket; a plain HTTP request to the same path keeps
/// the fallback semantics (forwarded to the sidecar over plain HTTP), so the
/// explicit route never changes behaviour for non-WS clients.
#[allow(clippy::too_many_arguments)] // axum extractor signature: guard, state, path, upgrade, query, method, headers, body
async fn desktop_ws_route(
    // Extracted by the route handlers below (first extractor): the per-session
    // runtime key guard, identical to every other gated runtime route.
    _auth: Authed,
    state: AppState,
    path: String,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = match query {
        Some(q) if !q.is_empty() => format!("{path}?{q}"),
        _ => path,
    };
    // Inactive lane: the pre-desktop behaviour (default empty 404) for both
    // upgrade and plain requests.
    if !state.config.desktop_lane_active() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match ws {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| relay_ws(state, path_and_query, socket)),
        // Non-upgrade request: plain-HTTP forward (fallback semantics).
        Err(_) => match forward_to_sidecar(&state, &path_and_query, method, &headers, body).await {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        },
    }
}

/// `GET /desktop/stream` — the desktop video stream (VNC/websocket).
pub async fn desktop_stream_ws(
    auth: Authed,
    State(state): State<AppState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    query: RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    desktop_ws_route(
        auth,
        state,
        "/desktop/stream".to_owned(),
        ws,
        query,
        method,
        headers,
        body,
    )
    .await
}

/// `GET /desktop/audio` — the desktop audio stream.
pub async fn desktop_audio_ws(
    auth: Authed,
    State(state): State<AppState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    query: RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    desktop_ws_route(
        auth,
        state,
        "/desktop/audio".to_owned(),
        ws,
        query,
        method,
        headers,
        body,
    )
    .await
}

/// `GET /events` — the sidecar's event stream.
pub async fn desktop_events_ws(
    auth: Authed,
    State(state): State<AppState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    query: RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    desktop_ws_route(
        auth,
        state,
        "/events".to_owned(),
        ws,
        query,
        method,
        headers,
        body,
    )
    .await
}

/// `GET /browser/view/{sid}/{token}/{channel}` — the sidecar's browser-view
/// signalling/stream surface (`channel` is passed through verbatim; the
/// sidecar is the authority on which channels exist).
#[allow(clippy::too_many_arguments)] // axum extractor signature: guard, state, path, upgrade, query, method, headers, body
pub async fn desktop_browser_view(
    auth: Authed,
    State(state): State<AppState>,
    axum::extract::Path((sid, token, channel)): axum::extract::Path<(String, String, String)>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    query: RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/browser/view/{sid}/{token}/{channel}");
    desktop_ws_route(auth, state, path, ws, query, method, headers, body).await
}

/// Bidirectional client↔sidecar WS relay (the runtime-side sibling of the
/// broker's `relay_to_upstream`): connect upstream with the sidecar Bearer,
/// then pump frames both ways until either side ends. Text, binary, ping and
/// pong frames are forwarded verbatim; close frames are forwarded WITH their
/// code/reason. No timers: an idle socket is never killed. Frame size caps on
/// both legs match the configured upload budget.
async fn relay_ws(state: AppState, path_and_query: String, mut client: WebSocket) {
    let upstream = match connect_sidecar_ws(&state, &path_and_query).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(%path_and_query, "desktop ws upstream connect failed: {e}");
            let _ = client
                .send(AxumMsg::Close(Some(CloseFrame {
                    code: 1011,
                    reason: Utf8Bytes::from_static("desktop sidecar unavailable"),
                })))
                .await;
            return;
        }
    };

    // The sidecar's /desktop/stream, /desktop/audio and /events expect a TEXT
    // auth frame ({type:"auth", token: <OPEN_TERMINAL_API_KEY>}) as the FIRST
    // message before any binary VNC data (open-terminal desktop.py stream():
    // receive_json within 10 s). The runtime hop authenticates via the upgrade
    // request's Authorization header, which the sidecar ignores — so inject the
    // auth frame here, before relaying anything from the client. Without this,
    // noVNC's first binary frame (RFB handshake) hits receive_json ->
    // KeyError("text") -> close 1011 -> the Desktop tab reconnect-loops.
    let mut upstream = upstream;
    if let Some(key) = state.config.desktop_api_key.as_deref() {
        let auth = TungMsg::Text(format!("{{\"type\":\"auth\",\"token\":\"{key}\"}}").into());
        if upstream.send(auth).await.is_err() {
            let _ = client
                .send(AxumMsg::Close(Some(CloseFrame {
                    code: 1011,
                    reason: Utf8Bytes::from_static("desktop auth failed"),
                })))
                .await;
            return;
        }
    }

    let (mut up_sink, mut up_stream) = upstream.split();
    let (mut client_sink, mut client_stream) = client.split();

    // Client → sidecar. Ending on Close / error / EOF; close frames are
    // forwarded upstream before the pump stops.
    let mut c2u = tokio::spawn(async move {
        while let Some(msg) = client_stream.next().await {
            match msg {
                Ok(m) => {
                    if up_sink.send(to_upstream(m)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Dropping up_sink closes the upstream socket.
    });

    // Sidecar → client.
    let mut u2c = tokio::spawn(async move {
        while let Some(msg) = up_stream.next().await {
            match msg {
                Ok(m) => match to_client(m) {
                    Some(c) => {
                        if client_sink.send(c).await.is_err() {
                            break;
                        }
                    }
                    None => break, // sidecar closed
                },
                Err(_) => break,
            }
        }
    });

    // First-completed-wins: the other pump is aborted and the dropped sinks
    // close both sockets.
    tokio::select! {
        _ = &mut c2u => {}
        _ = &mut u2c => {}
    }
    c2u.abort();
    u2c.abort();
}

/// Build the upstream WS handshake request for `path_and_query` against the
/// configured sidecar (`http`→`ws`, `https`→`wss`), inject the sidecar Bearer,
/// and connect with frame-size caps matching the upload budget.
async fn connect_sidecar_ws(
    state: &AppState,
    path_and_query: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let base = desktop_base_url(state);
    let ws_base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .unwrap_or(base);
    let url = format!("{ws_base}{path_and_query}");

    let mut request = url.into_client_request()?;
    if let Some(key) = state.config.desktop_api_key.as_deref() {
        match format!("Bearer {key}").parse::<HeaderValue>() {
            Ok(v) => {
                request.headers_mut().insert(header::AUTHORIZATION, v);
            }
            Err(e) => tracing::warn!("desktop api key is not a valid header value: {e}"),
        }
    }
    let budget = state.config.max_upload_bytes as usize;
    // Non-exhaustive struct: mutate the defaults instead of a literal.
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(budget);
    config.max_frame_size = Some(budget);
    let (stream, _resp) =
        tokio_tungstenite::connect_async_with_config(request, Some(config), true).await?;
    Ok(stream)
}

/// Convert an inbound (client→sidecar) axum message to a tungstenite message
/// (1:1 — even close frames are forwarded, so no `None` signalling is needed).
fn to_upstream(msg: AxumMsg) -> TungMsg {
    match msg {
        AxumMsg::Text(t) => TungMsg::Text(t.as_str().into()),
        AxumMsg::Binary(b) => TungMsg::Binary(b),
        AxumMsg::Ping(b) => TungMsg::Ping(b),
        AxumMsg::Pong(b) => TungMsg::Pong(b),
        AxumMsg::Close(frame) => TungMsg::Close(frame.map(|f| TungCloseFrame {
            // `Iana` wraps the raw numeric code — the value the client chose is
            // relayed verbatim.
            code: TungCloseFrameCode::Iana(f.code),
            reason: f.reason.as_str().into(),
        })),
    }
}

/// Convert an outbound (sidecar→client) tungstenite message to an axum
/// message. `None` signals the sidecar closed.
fn to_client(msg: TungMsg) -> Option<AxumMsg> {
    match msg {
        TungMsg::Text(t) => Some(AxumMsg::Text(t.as_str().into())),
        TungMsg::Binary(b) => Some(AxumMsg::Binary(b)),
        TungMsg::Ping(b) => Some(AxumMsg::Ping(b)),
        TungMsg::Pong(b) => Some(AxumMsg::Pong(b)),
        TungMsg::Close(frame) => Some(AxumMsg::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        }))),
        TungMsg::Frame(_) => None,
    }
}

// ---------------------------------------------------------------------------
// GET /api/config — merge runtime flags + sidecar flags
// ---------------------------------------------------------------------------

/// `GET /api/config` — merge the runtime's own feature flags with the sidecar's
/// (desktop lane) response. Runtime flags win key conflicts; sidecar-only keys
/// (desktop/browser/git/events) are added. With the lane inactive — or the
/// sidecar down/hung — this is exactly the runtime's own flags, and never a 5xx.
///
/// The sidecar fetch is single-flight (the cache lock is held across the
/// fetch) and cached for a few seconds, so client bursts don't each hit the
/// sidecar.
pub async fn merged_api_config(
    state: &AppState,
    runtime_flags: serde_json::Value,
) -> serde_json::Value {
    if !state.config.desktop_lane_active() {
        return runtime_flags;
    }

    let mut cache = state.desktop_config_cache.lock().await;
    if let Some(hit) = cache
        .as_ref()
        .filter(|c| c.fetched_at.elapsed() < state.config.desktop_config_cache_ttl())
    {
        return merge_configs(runtime_flags, &hit.payload);
    }

    let Some(payload) = fetch_sidecar_config(state).await else {
        // Sidecar down/hung/malformed: serve the runtime's flags unchanged.
        // The cache is deliberately left empty so the next request retries.
        return runtime_flags;
    };
    *cache = Some(DesktopConfigCache {
        fetched_at: Instant::now(),
        payload: payload.clone(),
    });
    merge_configs(runtime_flags, &payload)
}

/// Fetch `GET {DESKTOP_URL}/api/config` with the sidecar Bearer and a short
/// timeout. Any failure (transport, non-2xx, non-JSON) yields `None`.
async fn fetch_sidecar_config(state: &AppState) -> Option<serde_json::Value> {
    let url = format!("{}/api/config", desktop_base_url(state));
    let mut request = state
        .desktop_client
        .get(&url)
        .timeout(state.config.desktop_config_fetch_timeout());
    if let Some(key) = state.config.desktop_api_key.as_deref() {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
            request = request.header(header::AUTHORIZATION, v);
        }
    }
    let resp = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%url, "desktop sidecar /api/config fetch failed: {e}");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(%url, status = %resp.status(), "desktop sidecar /api/config returned an error");
        return None;
    }
    match resp.text().await {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| tracing::warn!("desktop sidecar /api/config was not valid JSON: {e}"))
            .ok(),
        Err(e) => {
            tracing::warn!(%url, "reading desktop sidecar /api/config failed: {e}");
            None
        }
    }
}

/// Merge the sidecar's config INTO the runtime's: every runtime key is kept
/// verbatim (runtime flags win conflicts), sidecar-only keys are added at
/// top level and inside `features`.
#[must_use]
fn merge_configs(mut runtime: serde_json::Value, sidecar: &serde_json::Value) -> serde_json::Value {
    let (Some(runtime_obj), Some(sidecar_obj)) = (runtime.as_object_mut(), sidecar.as_object())
    else {
        return runtime;
    };
    for (key, value) in sidecar_obj {
        if key == "features" {
            let Some(sidecar_features) = value.as_object() else {
                continue;
            };
            let runtime_features = runtime_obj
                .entry("features")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(features_obj) = runtime_features.as_object_mut() {
                for (fk, fv) in sidecar_features {
                    features_obj.entry(fk.clone()).or_insert(fv.clone());
                }
            }
        } else {
            runtime_obj.entry(key.clone()).or_insert(value.clone());
        }
    }
    runtime
}
