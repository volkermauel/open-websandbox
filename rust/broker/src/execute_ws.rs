//! WebSocket relays for the runtime's background-process streams (stage 3,
//! open-terminal v0.17.x / OWUI 0.13.x processes dock):
//!
//! * `GET /execute` — the live process-list stream (plain GET falls through
//!   to the catch-all proxy for the JSON list, same branch pattern as the
//!   terminal info/WS split).
//! * `GET /execute/{id}/status?offset=&tail=` — the per-process output
//!   stream (`offset`/`tail` forwarded verbatim; the socket pushes snapshots
//!   on every change and closes when the process finishes).
//!
//! Contract mirrors [`crate::terminal`]: identity from query params/headers
//! (`userId`, `sessionId`/`chatId`, `persistence` — NO path-session fallback:
//! the status path carries a PROCESS id, not the chat id), first-message
//! shared-secret auth (4001 close on bad token/timeout), resolve + mandatory
//! per-session runtime key (fail-closed 1011), upstream
//! `ws://<pod-ip>:8888<same path?same query>` with the per-session Bearer,
//! #158 idle-clock touches both directions. The query string (offset/tail)
//! is forwarded verbatim.

#![forbid(unsafe_code)]

use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request};
use axum::response::Response;

use crate::error::ApiError;
use crate::state::AppState;
use crate::terminal::{
    authenticate_first_message, relay_to_upstream_with_touch, resolve_and_key, SessionTouch,
    TerminalIdentity, TerminalWsQuery,
};

/// `GET /execute` — WebSocket upgrade ⇒ the process-list stream; anything
/// else proxies to the runtime's JSON list via the catch-all.
pub async fn execute_get_or_ws(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    State(state): State<AppState>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    if let Ok(upgrade) = ws {
        let identity = terminal_identity_no_path(&query, &headers, &state)?;
        let query_string = req.uri().query().unwrap_or("").to_owned();
        let path = req.uri().path().to_owned();
        return Ok(upgrade
            .on_upgrade(move |socket| relay_execute(socket, state, identity, path, query_string)));
    }
    crate::proxy::proxy_catch_all(crate::auth::Authed, State(state), req).await
}

/// `GET /execute/{id}/status` — WebSocket upgrade ⇒ the output stream;
/// anything else proxies to the runtime's JSON window.
pub async fn execute_status_get_or_ws(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    State(state): State<AppState>,
    Query(query): Query<TerminalWsQuery>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    if let Ok(upgrade) = ws {
        let identity = terminal_identity_no_path(&query, &headers, &state)?;
        let query_string = req.uri().query().unwrap_or("").to_owned();
        // Concrete request path: substitutes the real process id. The literal
        // template "/execute/{id}/status" dialed a 404/4004 on every stream.
        let path = req.uri().path().to_owned();
        return Ok(upgrade
            .on_upgrade(move |socket| relay_execute(socket, state, identity, path, query_string)));
    }
    crate::proxy::proxy_catch_all(crate::auth::Authed, State(state), req).await
}

/// Identity without a path-session fallback (desktop-relay semantics): the
/// `/execute*` paths carry process ids, never the chat id.
/// `upstream_path` is the request path, so it always starts with `/`; the join
/// must NOT add another slash — `ws://ip:8888//execute` 404s on the runtime
/// (issue #219, production 2026-11-23: OWUI 0.13.4 processes dock dead).
pub(crate) fn execute_upstream_url(pod_ip: &str, upstream_path: &str) -> String {
    format!(
        "ws://{pod_ip}:{}{upstream_path}",
        crate::proxy::RUNTIME_PORT
    )
}

fn terminal_identity_no_path(
    query: &TerminalWsQuery,
    headers: &HeaderMap,
    state: &AppState,
) -> Result<TerminalIdentity, ApiError> {
    crate::terminal::terminal_identity(query, headers, "", state.config.default_profile)
}

/// Post-upgrade relay: first-message auth → resolve → key → relay frames to
/// the runtime stream (same order as [`crate::desktop::relay_desktop`]).
async fn relay_execute(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    identity: TerminalIdentity,
    path: String,
    query_string: String,
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

    let upstream_path = if query_string.is_empty() {
        path
    } else {
        format!("{path}?{query_string}")
    };
    tracing::info!(
        user = %identity.user_id,
        session = %identity.session_id,
        sandbox = %resolved.name,
        pod_ip = %resolved.pod_ip,
        path = %upstream_path,
        "execute ws relay started"
    );

    // 3. #158: WS traffic refreshes the idle clock so the reaper cannot park
    //    an actively-used session mid-stream.
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

    // 4. connect the runtime stream and relay frames until either side ends.
    let upstream_url = execute_upstream_url(&resolved.pod_ip, &upstream_path);
    relay_to_upstream_with_touch(
        client,
        &upstream_url,
        &runtime_api_key,
        client_touch,
        upstream_touch,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::execute_upstream_url;

    #[test]
    fn upstream_url_is_single_slashed() {
        let url = execute_upstream_url("10.0.0.7", "/execute?user_id=u1");
        assert_eq!(url, "ws://10.0.0.7:8888/execute?user_id=u1");
        assert!(!url.contains("//e"), "double slash dials a 404: {url}");
    }

    #[test]
    fn upstream_url_status_stream_single_slashed() {
        let url = execute_upstream_url("10.0.0.7", "/execute/9/status?offset=0");
        assert_eq!(url, "ws://10.0.0.7:8888/execute/9/status?offset=0");
    }
}
