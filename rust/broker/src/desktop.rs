//! Desktop/browser/event WebSocket relays (OWUI desktop-tab contract).
//!
//! The sandbox pod serves the desktop/browser/event surfaces from a second
//! container (`open-terminal:0.16.1-desktop`, `:8000`; spike 2026-11-05, ns
//! `agent-sandbox-runtime`), reached through the in-sandbox runtime. The
//! broker owns the single OWUI connection, so these WS upgrades ride the SAME
//! contract as the terminal relay ([`crate::terminal`]):
//!
//! * identity from query params / headers (`userId`, `sessionId`/`chatId`,
//!   `persistence` → `terminal_identity`) with NO path-session fallback — the
//!   `/browser/view/{sid}/{token}/{channel}` path carries sidecar view ids,
//!   not the chat id. A missing user/session is the same pre-upgrade 400 the
//!   terminal identity check produces;
//! * first-message shared-secret auth (4001 close on a bad token or timeout);
//! * `resolve_sandbox` get-or-create + ready-poll, then the MANDATORY
//!   per-session runtime key (fail-closed 1011 — no shared-key fallback);
//! * upstream `ws://<pod-ip>:8888<same path?same query>` authenticated with
//!   that key's Bearer, the client's `Sec-WebSocket-Protocol` list forwarded
//!   verbatim (axum echoes the client's first offered back);
//! * #158 idle-clock touches in both directions so the reaper cannot park an
//!   open desktop session mid-use.
//!
//! Divergence from the terminal route, by construction: identity/channel
//! validation runs BEFORE the `WebSocketUpgrade` extraction, so a non-upgrade
//! request surfaces the canonical JSON 400 (`{"detail": ...}`) instead of
//! axum's plain-text upgrade rejection. For real upgrade requests behavior is
//! identical.
//!
//! The `/{*path}` catch-all stays plain HTTP (it strips `Upgrade`); these
//! explicit routes are the supported path for the desktop/browser/event
//! sockets. Like the terminal WS upgrade, they are opaque passthroughs and
//! intentionally omitted from the OpenAPI document (issue #75 Q5).

#![forbid(unsafe_code)]

use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request};
use axum::response::Response;

use crate::error::ApiError;
use crate::proxy::RUNTIME_PORT;
use crate::resolve::ResolvedSandbox;
use crate::state::AppState;
use crate::terminal::{
    authenticate_first_message, relay_to_upstream_with_headers, resolve_and_key, SessionTouch,
    TerminalIdentity, TerminalWsQuery,
};
use shared::Profile;

/// Channels the `/browser/view/{sid}/{token}/{channel}` route relays.
const BROWSER_CHANNELS: &[&str] = &["ws", "stream", "vnc"];

/// `GET /desktop/stream` (WebSocket upgrade) — the desktop VNC/stream socket.
pub async fn desktop_stream_ws(
    State(state): State<AppState>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    let pq = path_and_query(&req);
    desktop_ws(state, query, headers, req, pq).await
}

/// `GET /desktop/audio` (WebSocket upgrade) — the desktop audio socket.
pub async fn desktop_audio_ws(
    State(state): State<AppState>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    let pq = path_and_query(&req);
    desktop_ws(state, query, headers, req, pq).await
}

/// `GET /events` (WebSocket upgrade) — the desktop event socket.
pub async fn events_ws(
    State(state): State<AppState>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    let pq = path_and_query(&req);
    desktop_ws(state, query, headers, req, pq).await
}

/// `GET /browser/view/{sid}/{token}/{channel}` (WebSocket upgrade) — a browser
/// view socket. `{sid}`/`{token}` are sidecar view ids forwarded verbatim;
/// `channel` must be one of `ws`, `stream`, `vnc` (validated pre-upgrade, so a
/// garbage path never resolves a sandbox).
pub async fn browser_view_ws(
    State(state): State<AppState>,
    Path((_, _, channel)): Path<(String, String, String)>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    if !BROWSER_CHANNELS.contains(&channel.as_str()) {
        return Err(ApiError::BadRequest(format!(
            "unknown browser view channel {channel:?} (expected ws, stream or vnc)"
        )));
    }
    let pq = path_and_query(&req);
    desktop_ws(state, query, headers, req, pq).await
}

/// Shared handler body: validate the identity pre-upgrade (canonical 400 —
/// identical to [`crate::terminal::terminal_identity`]), echo the client's
/// offered subprotocols, then hand the upgraded socket to [`relay_desktop`].
async fn desktop_ws(
    state: AppState,
    query: TerminalWsQuery,
    headers: HeaderMap,
    req: Request<Body>,
    path_and_query: String,
) -> Result<Response, ApiError> {
    // No path-session fallback: these paths carry sidecar ids, not the chat id.
    let identity = terminal_identity_or_reject(&query, &headers, &state)?;
    let (mut parts, _) = req.into_parts();
    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &state)
        .await
        .map_err(|e| ApiError::BadRequest(format!("expected a WebSocket upgrade request: {e}")))?;
    // Pass the client's offered subprotocols straight through: echo the first
    // offered back (axum picks the first of OUR list the client requested) and
    // forward the full list on the upstream upgrade.
    let protocols: Vec<String> = ws
        .requested_protocols()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect();
    Ok(ws
        .protocols(protocols.iter().cloned())
        .on_upgrade(move |socket| {
            relay_desktop(socket, state, identity, path_and_query, protocols)
        }))
}

/// [`crate::terminal::terminal_identity`] with the canonical-400 mapping shared
/// by all four routes (the terminal route returns the same error inline).
/// Identity for the desktop/browser/event relays. Deliberately NOT
/// [`crate::terminal::terminal_identity`]: that helper rejects a missing
/// session, but the desktop is the user's sandbox desktop — when the caller has
/// no session of its own (Open WebUI sends an empty `X-Session-Id` for an
/// unsaved chat) we attach to the persistent session (`session = user`),
/// mirroring how the terminal path resolves the persistent profile for the same
/// chat.
pub fn desktop_identity(
    query: &TerminalWsQuery,
    headers: &HeaderMap,
    default_profile: Profile,
) -> Result<TerminalIdentity, ApiError> {
    let user_id = query
        .user_id
        .clone()
        .or_else(|| header_value(headers, "x-user-id"))
        .unwrap_or_default();
    if user_id.is_empty() {
        return Err(ApiError::BadRequest("user_id is required".to_string()));
    }
    let session_id = query
        .session_id
        .clone()
        .or_else(|| query.chat_id.clone())
        .or_else(|| header_value(headers, "x-session-id"))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| user_id.clone());
    Ok(TerminalIdentity {
        user_id,
        session_id,
        profile: default_profile,
    })
}

fn terminal_identity_or_reject(
    query: &TerminalWsQuery,
    headers: &HeaderMap,
    state: &AppState,
) -> Result<TerminalIdentity, ApiError> {
    let profile = state.config.default_profile;
    desktop_identity(query, headers, profile)
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .filter(|v| !v.is_empty())
}

/// Path (+ query) of the inbound request, forwarded verbatim towards the
/// runtime (same semantics as the catch-all proxy).
fn path_and_query(req: &Request<Body>) -> String {
    req.uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_owned())
}

/// Upstream WS URL for a desktop/browser/event relay:
/// `ws://<pod-ip>:8888<path?query>` (path + query preserved verbatim), or the
/// test/dev override base with its scheme swapped to `ws`/`wss` — the WS
/// counterpart of [`crate::proxy::upstream_url`].
#[must_use]
pub fn ws_upstream_url(
    state: &AppState,
    resolved: &ResolvedSandbox,
    path_and_query: &str,
) -> String {
    if let Some(base) = state.runtime_upstream_override.as_deref() {
        let base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}{path_and_query}")
    } else {
        format!("ws://{}:{RUNTIME_PORT}{path_and_query}", resolved.pod_ip)
    }
}

/// Post-upgrade relay: first-message auth → resolve + per-session key →
/// connect the upstream desktop WS (Bearer + subprotocols) → relay frames with
/// #158 idle-clock touches in both directions.
async fn relay_desktop(
    socket: WebSocket,
    state: AppState,
    identity: TerminalIdentity,
    path_and_query: String,
    protocols: Vec<String>,
) {
    // 1. first-message shared-secret auth (terminal contract).
    let mut client = socket;
    let secret = state.config.shared_secret.clone();
    if !authenticate_first_message(&mut client, &secret).await {
        return;
    }

    // 2. resolve the target sandbox + its mandatory per-session runtime key
    //    (closes the client socket 1011 on any failure).
    let Some((resolved, runtime_api_key)) = resolve_and_key(&mut client, &state, &identity).await
    else {
        return;
    };

    tracing::info!(
        user = %identity.user_id,
        session = %identity.session_id,
        sandbox = %resolved.name,
        pod_ip = %resolved.pod_ip,
        path = %path_and_query,
        "desktop ws relay started"
    );

    // 3. #158: WS traffic refreshes the idle clock so the reaper cannot park an
    //    actively-used desktop session mid-session (same throttled touches as
    //    the terminal relay).
    let ws_interval = Duration::from_secs(state.config.ws_touch_interval_seconds);
    let (client_touch, upstream_touch) = if ws_interval.is_zero() {
        (None, None)
    } else {
        (
            Some(SessionTouch::new(
                state.store.clone(),
                resolved.name.clone(),
                ws_interval,
                "client",
            )),
            Some(SessionTouch::new(
                state.store.clone(),
                resolved.name.clone(),
                ws_interval,
                "upstream",
            )),
        )
    };

    // 4. forward the client's offered subprotocols verbatim; the upstream
    //    picks from the same list the client sees echoed back.
    let mut extra = HeaderMap::new();
    if !protocols.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&protocols.join(", ")) {
            extra.insert("sec-websocket-protocol", v);
        }
    }

    let upstream_url = ws_upstream_url(&state, &resolved, &path_and_query);
    relay_to_upstream_with_headers(
        client,
        &upstream_url,
        &runtime_api_key,
        extra,
        client_touch,
        upstream_touch,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> ResolvedSandbox {
        ResolvedSandbox {
            last_offloaded_key: None,
            name: "owui-c-deadbeef".into(),
            pod_ip: "10.0.0.5".into(),
            restored_session: None,
            restored_pod: None,
            pod_uid: None,
            operating_mode: None,
        }
    }

    #[test]
    fn upstream_url_targets_pod_ip_and_8888_verbatim() {
        let state = AppState::for_test(shared::BrokerConfig::default());
        assert_eq!(
            ws_upstream_url(
                &state,
                &resolved(),
                "/desktop/stream?userId=u1&sessionId=c1"
            ),
            "ws://10.0.0.5:8888/desktop/stream?userId=u1&sessionId=c1"
        );
        // Browser view path (sidecar ids + token) forwarded verbatim.
        assert_eq!(
            ws_upstream_url(&state, &resolved(), "/browser/view/s-1/tok-2/vnc"),
            "ws://10.0.0.5:8888/browser/view/s-1/tok-2/vnc"
        );
        // A path without a query still works.
        assert_eq!(
            ws_upstream_url(&state, &resolved(), "/events"),
            "ws://10.0.0.5:8888/events"
        );
    }

    #[test]
    fn upstream_url_override_swaps_scheme_and_appends_verbatim() {
        let state = AppState::for_test(shared::BrokerConfig::default())
            .with_runtime_upstream_override("http://127.0.0.1:9999");
        assert_eq!(
            ws_upstream_url(&state, &resolved(), "/desktop/audio?q=1"),
            "ws://127.0.0.1:9999/desktop/audio?q=1"
        );
        let state = AppState::for_test(shared::BrokerConfig::default())
            .with_runtime_upstream_override("https://127.0.0.1:9999");
        assert_eq!(
            ws_upstream_url(&state, &resolved(), "/events"),
            "wss://127.0.0.1:9999/events"
        );
    }
}
