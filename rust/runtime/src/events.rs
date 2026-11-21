//! `WS /events` — the open-terminal 0.17.x change-notification channel behind
//! OWUI's live Files tab, ports list and git-panel refresh (`features.events`).
//!
//! # Contract (ported from `open_terminal/utils/events.py` + `main.py`)
//!
//! After the upgrade (per-session Bearer; the broker forwards OWUI's auth
//! frame, tolerated and ignored here) the client sends
//! `{"type":"subscribe","path": <dir>}` and/or `{"type":"subscribe",
//! "ports": true}` within 10 s — non-subscribe first frames close **4000**.
//! A later subscribe REPLACES the previous watch/port streams.
//!
//! * `{"type":"fs:ready"}` — once after the watch is installed.
//! * `{"type":"fs:change"}` — the watched tree changed; **no filenames**
//!   (clients only invalidate status), driven by a 1 s fingerprint poll of
//!   the directory (entries name/size/mtime + the directory's own mtime) and,
//!   when the watched directory sits inside a git repository, the `.git`
//!   HEAD/index/refs markers so commits and branch switches also fire.
//! * `{"type":"ports:change","ports":[…]}` — the session-visible listener
//!   set (same scan as `GET /ports`), pushed on the first scan and on every
//!   change (1 s poll).
//!
//! Close codes (upstream parity): **4000** invalid subscription, **4003**
//! watch path unavailable (escape/missing), **1011** watcher failure. The
//! upstream native watchfiles watcher (recursive over the repo root, shared
//! per path) is replaced by a per-connection poll — same wire semantics, no
//! inotify dependency (documented divergence).
//!
//! Watch-path resolution: `path` resolves against the session cwd
//! (`X-Session-Id`), confined by [`crate::safe_path`] like every file route —
//! an escape closes **4003** (upstream's PermissionError → 4003 path).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::auth::Authed;
use crate::files::base_of;
use crate::safe_path::safe_path;
use crate::state::AppState;

/// Subscribe deadline for the first frame (upstream `wait_for(..., 10)`).
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Fingerprint poll cadence for the fs watch.
const FS_POLL: Duration = Duration::from_secs(1);
/// Ports scan cadence while a ports subscription is live.
const PORTS_POLL: Duration = Duration::from_secs(1);
/// Close code: invalid subscribe frame / subscription payload.
const CLOSE_INVALID: u16 = 4000;
/// Close code: watch path unavailable (escape or missing directory).
const CLOSE_UNAVAILABLE: u16 = 4003;

/// One entry in the fs fingerprint (relative name → size + mtime nanos).
type Fingerprint = BTreeMap<String, (u64, i64)>;

/// `GET /events` — WebSocket upgrade for the change stream.
pub async fn events_ws(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    match ws {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| stream(state, headers, socket)),
        Err(_) => axum::http::StatusCode::BAD_REQUEST.into_response(),
    }
}

/// The stream loop: subscribe frames replace the watch/port tasks.
async fn stream(state: AppState, headers: HeaderMap, mut socket: WebSocket) {
    // The broker forwards OWUI's auth frame first — tolerate (auth already
    // ran at upgrade via the per-session Bearer).
    let _ = tokio::time::timeout(SUBSCRIBE_TIMEOUT, socket.recv()).await;

    let Ok(base) = base_of(&state, &headers) else {
        close(&mut socket, CLOSE_INVALID, "Invalid session").await;
        return;
    };

    let mut fs_watch: Option<(PathBuf, Fingerprint)> = None;
    let mut ports_watch = false;
    let mut last_ports: Option<String> = None;
    let mut fs_tick = tokio::time::interval(FS_POLL);
    fs_tick.tick().await; // skip the immediate tick
    let mut ports_tick = tokio::time::interval(PORTS_POLL);
    ports_tick.tick().await; // skip the immediate tick

    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    match parse_subscribe(&text) {
                        Ok(sub) => {
                            // A `path` subscribe REPLACES the fs watch (upstream
                            // stops the previous watcher task first).
                            if sub.path.is_some() {
                                match resolve_watch_dir(&base, sub.path.as_deref()) {
                                    Ok(dir) => {
                                        let fingerprint = fingerprint(&dir);
                                        fs_watch = Some((dir, fingerprint));
                                        if socket.send(Message::Text(
                                            json!({ "type": "fs:ready" }).to_string().into(),
                                        ))
                                        .await
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        close(&mut socket, CLOSE_UNAVAILABLE, &e).await;
                                        return;
                                    }
                                }
                            } else {
                                fs_watch = None;
                            }
                            ports_watch = sub.ports;
                            if !ports_watch {
                                last_ports = None;
                            }
                        }
                        Err(reason) => {
                            close(&mut socket, CLOSE_INVALID, &reason).await;
                            return;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                _ => {}
            },
            _ = fs_tick.tick(), if fs_watch.is_some() => {
                let Some((dir, seen)) = fs_watch.as_ref() else { continue };
                let current = fingerprint(dir);
                if &current != seen {
                    if let Some((_, seen)) = fs_watch.as_mut() {
                        *seen = current;
                    }
                    if socket
                        .send(Message::Text(json!({ "type": "fs:change" }).to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            _ = ports_tick.tick(), if ports_watch => {
                let mut snapshot = crate::ports::visible_ports().await;
                // Deterministic order so the string compare only fires on real
                // changes (the /proc scan order is not stable).
                snapshot.sort_by_key(|p| p.port);
                let rendered = serde_json::to_string(&snapshot).unwrap_or_default();
                if last_ports.as_deref() != Some(rendered.as_str()) {
                    last_ports = Some(rendered);
                    if socket
                        .send(Message::Text(
                            json!({ "type": "ports:change", "ports": snapshot }).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

/// The subscribe frame: an optional `path` (string) and/or `ports: true`.
struct Subscribe {
    path: Option<String>,
    ports: bool,
}

/// Parse + validate a subscribe frame (upstream's exact validations: non-
/// subscribe types close 4000; `path` must be a non-empty string ≤ 4096;
/// `ports` must be a bool).
fn parse_subscribe(text: &str) -> Result<Subscribe, String> {
    let payload: Value =
        serde_json::from_str(text).map_err(|_| String::from("Invalid watch request"))?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("subscribe") {
        return Err("Expected subscribe message".into());
    }
    let path = match payload.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.is_empty() || s.len() > 4096 {
                return Err("Invalid subscription".into());
            }
            Some(s.clone())
        }
        Some(_) => return Err("Invalid subscription".into()),
    };
    let ports = match payload.get("ports").unwrap_or(&Value::Bool(false)) {
        Value::Bool(b) => *b,
        _ => return Err("Invalid subscription".into()),
    };
    if path.is_none() && !ports {
        return Err("Invalid subscription".into());
    }
    Ok(Subscribe { path, ports })
}

/// Resolve the watch directory against the workspace base (absolute paths
/// inside the base are honoured — `safe_path` semantics), 4003 on escape or
/// when it is not a directory.
fn resolve_watch_dir(base: &Path, path: Option<&str>) -> Result<PathBuf, String> {
    let Some(raw) = path else {
        return Err("Invalid subscription".into());
    };
    let dir = safe_path(raw, base).map_err(|_| "Watch path unavailable".to_string())?;
    if !dir.is_dir() {
        return Err("Watch path unavailable".to_string());
    }
    Ok(dir)
}

/// The watched-tree fingerprint: sorted `name → (size, mtime nanos)` for the
/// directory's immediate entries PLUS the directory's own mtime PLUS, when the
/// directory sits inside a git repository, the `.git` HEAD/index mtimes — so
/// commits and branch switches fire `fs:change` too (upstream watches the
/// repo root recursively; our poll covers the observable signals without
/// inotify).
fn fingerprint(dir: &Path) -> Fingerprint {
    let mut map = Fingerprint::new();
    let stamp = |p: &Path| -> (u64, i64) {
        match std::fs::metadata(p) {
            Ok(m) => {
                let size = m.len();
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos() as i64);
                (size, mtime)
            }
            Err(_) => (0, 0),
        }
    };
    map.insert("(dir)".into(), stamp(dir));
    let Ok(read) = std::fs::read_dir(dir) else {
        return map;
    };
    for entry in read.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        map.insert(name, stamp(&entry.path()));
    }
    // Git markers: the nearest `.git` upwards (dir itself, then ancestors up
    // to the workspace root).
    let mut cursor = Some(dir);
    while let Some(c) = cursor {
        let git = c.join(".git");
        if git.exists() {
            for marker in ["HEAD", "index"] {
                map.insert(format!("(git/{marker})"), stamp(&git.join(marker)));
            }
            break;
        }
        cursor = c.parent();
    }
    map
}

/// Graceful close with code + reason.
async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_frames_parse_like_upstream() {
        let s = parse_subscribe(r#"{"type":"subscribe","path":"/workspace/app"}"#).unwrap();
        assert_eq!(s.path.as_deref(), Some("/workspace/app"));
        assert!(!s.ports);
        let s = parse_subscribe(r#"{"type":"subscribe","ports":true}"#).unwrap();
        assert_eq!(s.path, None);
        assert!(s.ports);
        // Both together are valid (upstream replaces fs watch, keeps ports).
        let s = parse_subscribe(r#"{"type":"subscribe","path":"sub","ports":true}"#).unwrap();
        assert_eq!(s.path.as_deref(), Some("sub"));
        assert!(s.ports);
        // Non-subscribe type, malformed JSON, wrong types → errors.
        assert!(parse_subscribe(r#"{"type":"ping"}"#).is_err());
        assert!(parse_subscribe("not json").is_err());
        assert!(parse_subscribe(r#"{"type":"subscribe","path":42}"#).is_err());
        assert!(parse_subscribe(r#"{"type":"subscribe","path":""}"#).is_err());
        assert!(parse_subscribe(r#"{"type":"subscribe","ports":"yes"}"#).is_err());
        // Empty subscription (neither path nor ports) is invalid.
        assert!(parse_subscribe(r#"{"type":"subscribe"}"#).is_err());
    }

    #[test]
    fn fingerprint_detects_changes_and_git_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, b"hello").unwrap();
        let f1 = fingerprint(dir.path());
        assert!(f1.contains_key("a.txt"));
        assert_eq!(f1.get("a.txt").map(|(s, _)| *s), Some(5));

        // Content change → different fingerprint.
        std::fs::write(&a, b"hello world").unwrap();
        let f2 = fingerprint(dir.path());
        assert_ne!(f1, f2);

        // A git marker dir registers HEAD/index keys (missing files stamp 0).
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: main\n").unwrap();
        let f3 = fingerprint(dir.path());
        assert!(f3.contains_key("(git/HEAD)"));
        assert!(f3.contains_key("(git/index)"));

        // New file → different fingerprint.
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        assert_ne!(f3, fingerprint(dir.path()));
    }
}
