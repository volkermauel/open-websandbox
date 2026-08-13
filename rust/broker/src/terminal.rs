//! Terminal WebSocket relay (OWUI open-terminal contract).
//!
//! Handler for the `/api/terminals/{session_id}` WebSocket relay.
//! Browsers cannot set arbitrary headers on a WS open, so the OWUI terminal UI
//! sends its identity via query params (`user_id`, `session_id`/`chat_id`,
//! `persistence`) with the chat id in the path, and the shared secret as the
//! first `{"type":"auth","token":...}` text message. After validating those we
//! `resolve_sandbox`, open an outbound WS to
//! the runtime pod's terminal endpoint (`ws://<pod-ip>:8888/api/terminals/{id}`),
//! and relay frames bidirectionally until either side closes (first-completed-
//! wins).
//!
//! The HTTP terminal-management surface (`POST /api/terminals`, `GET
//! /api/terminals/{id}` status) flows through the catch-all reverse proxy
//! ([`crate::proxy`]); only the interactive WS is handled here.
//!
//! The byte relay is exercised end-to-end (a live WS
//! fixture would need a PTY-equivalent upstream); the auth + identity parsing is
//! unit-tested below.

#![forbid(unsafe_code)]

use std::time::Duration;

use axum::extract::ws::{Message as AxumMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message as TungMsg;

use shared::{constant_time_eq, is_placeholder_secret, Profile};

use crate::error::ApiError;
use crate::proxy::{profile_from_header, RUNTIME_PORT};
use crate::resolve::resolve_sandbox;

use crate::metrics::AUTH_FAILURES_TOTAL;
use crate::state::AppState;

/// How long to wait for the OWUI first-message auth before closing (10s).
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Identity carried from the OWUI WS open (query params fall back to headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalIdentity {
    pub user_id: String,
    pub session_id: String,
    pub profile: Profile,
}

/// Query params OWUI may send on the WS open.
#[derive(Deserialize, Default, Debug)]
pub struct TerminalWsQuery {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub persistence: Option<String>,
}

/// Resolve the terminal identity from query params, then headers, then the path
/// session id. `X-User-Id` is required (1008
/// close when absent); `X-Session-Id`/chat falls back to the path id; profile
/// resolves via `profile_from_header`.
pub fn terminal_identity(
    query: &TerminalWsQuery,
    headers: &HeaderMap,
    path_session_id: &str,
    default_profile: Profile,
) -> Result<TerminalIdentity, ApiError> {
    let user_id = query
        .user_id
        .clone()
        .or_else(|| header_str(headers, "x-user-id").map(str::to_owned))
        .unwrap_or_default();
    if user_id.is_empty() {
        return Err(ApiError::BadRequest(
            "user_id and session_id are required".to_string(),
        ));
    }
    let session_id = query
        .session_id
        .clone()
        .or_else(|| query.chat_id.clone())
        .or_else(|| header_str(headers, "x-session-id").map(str::to_owned))
        .unwrap_or_else(|| path_session_id.to_string());
    if session_id.is_empty() {
        return Err(ApiError::BadRequest(
            "user_id and session_id are required".to_string(),
        ));
    }
    let profile = query
        .persistence
        .as_deref()
        .map(str::to_ascii_lowercase)
        .map_or_else(
            || profile_from_header(headers, default_profile),
            |p| match p.as_str() {
                "persistent" => Profile::Persistent,
                "ephemeral" => Profile::Ephemeral,
                _ => default_profile,
            },
        );
    Ok(TerminalIdentity {
        user_id,
        session_id,
        profile,
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
}

/// `GET /api/terminals/{id}` (WebSocket upgrade). Validates identity, then hands
/// off to `relay` inside the upgrade future (post-accept auth + resolve + relay).
pub async fn terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let identity = terminal_identity(&query, &headers, &session_id, state.config.default_profile)?;
    Ok(ws.on_upgrade(move |socket| relay(socket, state, identity, session_id)))
}

/// Post-upgrade relay: auth first message → resolve → ensure PTY → connect
/// upstream → relay frames. Closes the client socket with a reason on any failure.
async fn relay(socket: WebSocket, state: AppState, identity: TerminalIdentity, session_id: String) {
    // 1. first-message shared-secret auth (only when the secret is configured).
    let mut client = socket;
    let secret = state.config.shared_secret.clone();
    if !is_placeholder_secret(&secret) {
        match wait_auth(&mut client, &secret).await {
            AuthResult::Ok => {}
            AuthResult::Deny => {
                metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => "bad_token").increment(1);
                let _ = client
                    .send(AxumMsg::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4001,
                        reason: "invalid api key".into(),
                    })))
                    .await;
                return;
            }
            AuthResult::Timeout => {
                metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => "auth_timeout").increment(1);
                let _ = client
                    .send(AxumMsg::Close(Some(axum::extract::ws::CloseFrame {
                        code: 4001,
                        reason: "auth timeout or invalid payload".into(),
                    })))
                    .await;
                return;
            }
        }
    }

    // 2. resolve the target sandbox (close 1011 on failure).
    let resolved = match resolve_sandbox(
        &state,
        &identity.user_id,
        &identity.session_id,
        identity.profile,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = client
                .send(AxumMsg::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: format!("sandbox unavailable: {e}").into(),
                })))
                .await;
            return;
        }
    };

    // 3. best-effort: ensure an interactive PTY exists on the resolved pod before
    //    attaching (idempotent; errors suppressed).
    ensure_pty(&state, &resolved.pod_ip, &session_id).await;

    tracing::info!(
        user = %identity.user_id,
        session = %identity.session_id,
        sandbox = %resolved.name,
        pod_ip = %resolved.pod_ip,
        "terminal ws relay started"
    );
    // 4. connect the upstream runtime terminal WS (plaintext in-cluster;
    //    TLS terminates at the ingress) and relay frames until either side
    //    ends. The connect + bidirectional pump live in [`relay_to_upstream`]
    //    so the byte relay is exercisable end-to-end against a local echo
    //    server in `tests/ws_relay.rs`.
    let upstream_url = format!(
        "ws://{}:{RUNTIME_PORT}/api/terminals/{session_id}",
        resolved.pod_ip
    );
    relay_to_upstream(client, &upstream_url).await;
}

/// Connect the upstream runtime terminal WS (`upstream_url`) and relay frames
/// bidirectionally with the OWUI client `WebSocket` until either side closes
/// (first-completed-wins; the other pump task is aborted). On upstream-connect
/// failure the client socket is closed with 1011.
///
/// Factored out of `relay` (steps 4–5) as a test seam: the rest of `relay`
/// needs an [`AppState`] plus a resolved sandbox pod, which is heavier than a
/// test wires up, but this connect + pump path is fully exercisable against a
/// local in-process echo server (see `tests/ws_relay.rs`).
pub async fn relay_to_upstream(mut client: WebSocket, upstream_url: &str) {
    let upstream = match tokio_tungstenite::connect_async(upstream_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            tracing::warn!(%upstream_url, "terminal ws upstream connect failed: {e}");
            let _ = client
                .send(AxumMsg::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: "terminal unavailable".into(),
                })))
                .await;
            return;
        }
    };

    // Bidirectional relay until either side ends.
    let (mut up_sink, mut up_stream) = upstream.split();
    let (mut client_sink, mut client_stream) = client.split();

    let mut c2u = tokio::spawn(async move {
        while let Some(msg) = client_stream.next().await {
            match msg {
                Ok(m) => match to_upstream(m) {
                    Some(u) => {
                        if up_sink.send(u).await.is_err() {
                            break;
                        }
                    }
                    None => break, // client closed
                },
                Err(_) => break,
            }
        }
        // Dropping up_sink closes the upstream WS → the runtime's handler reaches
        // its finally-block PTY cleanup.
    });
    let mut u2c = tokio::spawn(async move {
        while let Some(msg) = up_stream.next().await {
            match msg {
                Ok(m) => match to_client(m) {
                    Some(c) => {
                        if client_sink.send(c).await.is_err() {
                            break;
                        }
                    }
                    None => break, // upstream closed
                },
                Err(_) => break,
            }
        }
    });

    // Stop as soon as EITHER side ends; abort the other and let the dropped sinks
    // close both sockets (first-completed-wins).
    tokio::select! {
        _ = &mut c2u => {}
        _ = &mut u2c => {}
    }
    c2u.abort();
    u2c.abort();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthResult {
    Ok,
    Deny,
    Timeout,
}

/// Read the first text message and validate `{"type":"auth","token":<secret>}`
/// (constant-time compare), within [`AUTH_TIMEOUT`].
async fn wait_auth(client: &mut WebSocket, secret: &str) -> AuthResult {
    let raw = match tokio::time::timeout(AUTH_TIMEOUT, client.recv()).await {
        Ok(Some(Ok(m))) => m
            .into_text()
            .map(|t| t.as_str().to_owned())
            .unwrap_or_default(),
        _ => return AuthResult::Timeout,
    };
    let payload: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return AuthResult::Timeout,
    };
    if payload.get("type").and_then(|v| v.as_str()) == Some("auth")
        && payload
            .get("token")
            .and_then(|v| v.as_str())
            .is_some_and(|t| constant_time_eq(t.as_bytes(), secret.as_bytes()))
    {
        AuthResult::Ok
    } else {
        AuthResult::Deny
    }
}

/// Best-effort `POST /api/terminals` to the resolved pod so an interactive PTY
/// exists before the WS attaches (idempotent; errors swallowed).
async fn ensure_pty(state: &AppState, pod_ip: &str, session_id: &str) {
    let mut headers = HeaderMap::new();
    if let Ok(v) = "application/json".parse() {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    if !state.config.runtime_api_key.is_empty() {
        if let Ok(v) = format!("Bearer {}", state.config.runtime_api_key).parse() {
            headers.insert(axum::http::header::AUTHORIZATION, v);
        }
    }
    if let Ok(v) = session_id.parse() {
        headers.insert("x-session-id", v);
    }
    // Best-effort (errors swallowed): ensures an interactive PTY exists on the
    // resolved pod before the WS attaches. The runtime key is the C-2 shared
    // key; C-3 resolves this pod's per-session Secret instead.
    let url = format!("http://{pod_ip}:{RUNTIME_PORT}/api/terminals");
    let _ = state.http.post(&url).headers(headers).send().await;
}

/// Convert an inbound (client→upstream) axum message to a tungstenite message.
/// `None` signals the client closed (the relay stops forwarding).
fn to_upstream(msg: AxumMsg) -> Option<TungMsg> {
    match msg {
        AxumMsg::Text(t) => Some(TungMsg::Text(t.as_str().into())),
        AxumMsg::Binary(b) => Some(TungMsg::Binary(b)),
        AxumMsg::Ping(b) => Some(TungMsg::Ping(b)),
        AxumMsg::Pong(b) => Some(TungMsg::Pong(b)),
        AxumMsg::Close(_) => None,
    }
}

/// Convert an outbound (upstream→client) tungstenite message to an axum message.
fn to_client(msg: TungMsg) -> Option<AxumMsg> {
    match msg {
        TungMsg::Text(t) => Some(AxumMsg::Text(t.as_str().into())),
        TungMsg::Binary(b) => Some(AxumMsg::Binary(b)),
        TungMsg::Ping(b) => Some(AxumMsg::Ping(b)),
        TungMsg::Pong(b) => Some(AxumMsg::Pong(b)),
        TungMsg::Close(_) | TungMsg::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn q(
        user: Option<&str>,
        session: Option<&str>,
        chat: Option<&str>,
        persist: Option<&str>,
    ) -> TerminalWsQuery {
        TerminalWsQuery {
            user_id: user.map(str::to_owned),
            session_id: session.map(str::to_owned),
            chat_id: chat.map(str::to_owned),
            persistence: persist.map(str::to_owned),
        }
    }

    fn hdr(name: &str, val: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            name.parse::<axum::http::HeaderName>().unwrap(),
            HeaderValue::from_str(val).unwrap(),
        );
        h
    }

    #[test]
    fn identity_requires_user() {
        let err = terminal_identity(
            &q(None, None, None, None),
            &HeaderMap::new(),
            "chat-1",
            Profile::Persistent,
        )
        .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn identity_session_falls_back_to_path_id() {
        let id = terminal_identity(
            &q(Some("u"), None, None, None),
            &HeaderMap::new(),
            "chat-9",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.user_id, "u");
        assert_eq!(id.session_id, "chat-9");
    }

    #[test]
    fn identity_prefers_query_then_header_then_path() {
        // query session wins over header/path.
        let id = terminal_identity(
            &q(Some("u"), Some("q-sess"), None, None),
            &hdr("x-session-id", "h-sess"),
            "path-sess",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.session_id, "q-sess");
        // chat_id is the second preference (OWUI chat id).
        let id = terminal_identity(
            &q(Some("u"), None, Some("chat-x"), None),
            &hdr("x-session-id", "h-sess"),
            "path-sess",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.session_id, "chat-x");
        // header over path.
        let id = terminal_identity(
            &q(Some("u"), None, None, None),
            &hdr("x-session-id", "h-sess"),
            "path-sess",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.session_id, "h-sess");
    }

    #[test]
    fn identity_user_from_header_when_query_absent() {
        let id = terminal_identity(
            &q(None, None, None, None),
            &hdr("x-user-id", "hdr-u"),
            "s",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.user_id, "hdr-u");
        assert_eq!(id.session_id, "s");
    }

    #[test]
    fn identity_profile_from_query_persistence() {
        let id = terminal_identity(
            &q(Some("u"), Some("s"), None, Some("ephemeral")),
            &HeaderMap::new(),
            "s",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.profile, Profile::Ephemeral);
        // Unknown persistence → default.
        let id = terminal_identity(
            &q(Some("u"), Some("s"), None, Some("bogus")),
            &HeaderMap::new(),
            "s",
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.profile, Profile::Persistent);
    }

    #[test]
    fn convert_messages_both_directions() {
        // Text + binary round-trip through both converters.
        let up = to_upstream(AxumMsg::Text("hi".into())).unwrap();
        assert!(matches!(up, TungMsg::Text(_)), "{up:?}");
        let up = to_upstream(AxumMsg::Binary(vec![1, 2, 3].into())).unwrap();
        assert!(matches!(up, TungMsg::Binary(_)));
        // Close → None (relay stop signal).
        assert_eq!(to_upstream(AxumMsg::Close(None)), None);

        let dn = to_client(TungMsg::Text("yo".into())).unwrap();
        assert!(matches!(dn, AxumMsg::Text(_)), "{dn:?}");
        let dn = to_client(TungMsg::Binary(vec![9].into())).unwrap();
        assert!(matches!(dn, AxumMsg::Binary(_)));
        assert_eq!(to_client(TungMsg::Close(None)), None);
    }
}
