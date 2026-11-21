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

/// One entry in the fs fingerprint: relative path → `(size, mtime nanos)`.
/// File SIZE makes content edits deterministic even on coarse-mtime
/// filesystems (tmpfs); directories carry size 0 and are detected by mtime.
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

    // The FIRST subscribe must arrive within 10 s (upstream closes 4000).
    // Auth frames (`{"type":"auth",…}`) are tolerated and skipped anywhere
    // before it: through the broker the relay consumes OWUI's auth frame and
    // forwards only what follows, but a direct client (or a relay change)
    // may put the auth frame in front of the subscribe — treating the first
    // frame unconditionally as the auth swallowed it (review finding 1).
    let deadline = tokio::time::Instant::now() + SUBSCRIBE_TIMEOUT;
    let first_subscribe = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            close(&mut socket, CLOSE_INVALID, "Expected subscribe message").await;
            return;
        }
        if let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout(remaining, socket.recv()).await
        {
            let parsed = serde_json::from_str::<Value>(&text).ok();
            let is_auth = parsed
                .as_ref()
                .and_then(|v| v.get("type").and_then(Value::as_str))
                == Some("auth");
            if is_auth {
                continue; // tolerated, keep waiting for the subscribe
            }
            if let Ok(sub) = parse_subscribe(&text) {
                break sub;
            }
            close(&mut socket, CLOSE_INVALID, "Expected subscribe message").await;
            return;
        }
        close(&mut socket, CLOSE_INVALID, "Expected subscribe message").await;
        return;
    };
    if apply_subscribe(
        &mut socket,
        &base,
        &mut fs_watch,
        &mut ports_watch,
        &mut last_ports,
        first_subscribe,
    )
    .await
    {
        return; // closed with an error
    }

    loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    // Auth frames mid-stream are tolerated (ignored).
                    let is_auth = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
                        .as_deref()
                        == Some("auth");
                    if is_auth {
                        continue;
                    }
                    match parse_subscribe(&text) {
                        Ok(sub) => {
                            if apply_subscribe(
                                &mut socket,
                                &base,
                                &mut fs_watch,
                                &mut ports_watch,
                                &mut last_ports,
                                sub,
                            )
                            .await
                            {
                                return;
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

/// Apply one parsed subscribe: install/replace the fs watch (+ `fs:ready`)
/// and/or the ports stream. Returns `true` when the socket was closed with an
/// error.
async fn apply_subscribe(
    socket: &mut WebSocket,
    base: &Path,
    fs_watch: &mut Option<(PathBuf, Fingerprint)>,
    ports_watch: &mut bool,
    last_ports: &mut Option<String>,
    sub: Subscribe,
) -> bool {
    // A `path` subscribe REPLACES the fs watch (upstream stops the previous
    // watcher task first); an EMPTY subscribe unsubscribes everything —
    // upstream accepts it as a no-op and keeps the socket open.
    if sub.path.is_some() {
        match resolve_watch_dir(base, sub.path.as_deref().unwrap_or(".")) {
            Ok(dir) => {
                let fingerprint = fingerprint(&dir);
                *fs_watch = Some((dir, fingerprint));
                if socket
                    .send(Message::Text(
                        json!({ "type": "fs:ready" }).to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    return true;
                }
            }
            Err(e) => {
                close(socket, CLOSE_UNAVAILABLE, &e).await;
                return true;
            }
        }
    } else {
        *fs_watch = None;
    }
    *ports_watch = sub.ports;
    if !*ports_watch {
        *last_ports = None;
    }
    false
}

/// Parse + validate a subscribe frame (upstream's exact validations: non-
/// subscribe types close 4000; `path` must be a non-empty string ≤ 4096;
/// `ports` must be a bool). An EMPTY subscribe is VALID — upstream treats it
/// as "unsubscribe everything" and keeps the socket open.
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
    Ok(Subscribe { path, ports })
}

/// Resolve the watch directory against the workspace base (absolute paths
/// inside the base are honoured — `safe_path` semantics), 4003 on escape or
/// when it is not a directory.
fn resolve_watch_dir(base: &Path, raw: &str) -> Result<PathBuf, String> {
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
/// Entry cap for the recursive walk — a workspace larger than this degrades
/// to the bounded prefix (documented divergence).
const FINGERPRINT_CAP: usize = 10_000;

/// The watched-tree fingerprint: a BOUNDED RECURSIVE walk of `path → mtime`
/// (files + directories, `.git` trees skipped but re-added as explicit
/// markers), so edits in subdirectories fire — the core live-Files behavior
/// upstream's recursive native watcher provides. Git state is covered by
/// explicit markers: `.git/HEAD`, the RESOLVED current-ref file
/// (`.git/refs/heads/<branch>`, so commits fire — a commit moves the ref, not
/// HEAD), `.git/index` and `.git/ORIG_HEAD`. For linked worktrees (`.git` is
/// a `gitdir:` file) the resolved gitdir's markers are watched only when they
/// sit inside the workspace (upstream skips out-of-workspace trees too).
fn fingerprint(dir: &Path) -> Fingerprint {
    let stamp = |p: &Path| -> (u64, i64) {
        match std::fs::metadata(p) {
            Ok(m) => {
                let size = if m.is_dir() { 0 } else { m.len() };
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
    let mut map = Fingerprint::new();
    // Bounded depth-first walk (relative paths as keys keep the map small).
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = stack.pop() {
        if visited >= FINGERPRINT_CAP {
            break;
        }
        let rel = current
            .strip_prefix(dir)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        map.insert(rel, stamp(&current));
        visited += 1;
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            if name == ".git" {
                // Git internals are covered by the explicit markers below.
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                if visited >= FINGERPRINT_CAP {
                    break;
                }
                let rel = path
                    .strip_prefix(dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                map.insert(rel, stamp(&path));
                visited += 1;
            }
        }
    }
    // Git markers: the nearest `.git` upwards (dir itself, then ancestors up
    // to the workspace root). `HEAD` alone misses commits (a commit moves the
    // branch REF, not HEAD) — resolve the ref file and stamp it too.
    let mut cursor = Some(dir);
    while let Some(c) = cursor {
        let git = c.join(".git");
        if !git.exists() {
            cursor = c.parent();
            continue;
        }
        if git.is_dir() {
            for marker in ["HEAD", "index", "ORIG_HEAD"] {
                map.insert(format!("(git/{marker})"), stamp(&git.join(marker)));
            }
            // Resolve `ref: refs/heads/<branch>` → the ref file itself.
            if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
                if let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") {
                    map.insert(
                        "(git/ref)".into(),
                        stamp(&git.join("refs/heads").join(branch.trim())),
                    );
                }
            }
        } else if let Ok(link) = std::fs::read_to_string(&git) {
            // Linked worktree: `.git` is a `gitdir: <path>` file pointing at
            // the real git dir (usually under the main repo's `.git/worktrees/`).
            if let Some(target) = link.trim().strip_prefix("gitdir: ") {
                let gitdir = PathBuf::from(target.trim());
                let confined =
                    gitdir.starts_with(dir) || gitdir.starts_with(dir.parent().unwrap_or(dir));
                if confined {
                    for marker in ["HEAD", "index", "ORIG_HEAD"] {
                        map.insert(format!("(git/{marker})"), stamp(&gitdir.join(marker)));
                    }
                    if let Ok(head) = std::fs::read_to_string(gitdir.join("HEAD")) {
                        if let Some(branch) = head.trim().strip_prefix("ref: ") {
                            map.insert("(git/ref)".into(), stamp(&gitdir.join(branch.trim())));
                        }
                    }
                }
            }
        }
        break;
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
        // An EMPTY subscribe is VALID — upstream treats it as
        // "unsubscribe everything" and keeps the socket open.
        let empty = parse_subscribe(r#"{"type":"subscribe"}"#).unwrap();
        assert_eq!(empty.path, None);
        assert!(!empty.ports);
    }

    #[test]
    fn fingerprint_detects_changes_nested_and_git_markers() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.rs");
        std::fs::write(&a, "hello\n").unwrap();
        let f1 = fingerprint(dir.path());
        // NESTED entries are in the fingerprint (mtime keys).
        assert!(f1.contains_key("src/a.rs"), "{:?}", f1.keys());
        assert_eq!(f1.get("src/a.rs").map(|(s, _)| *s), Some(6));

        // A nested edit changes the fingerprint (root mtime does NOT move).
        std::fs::write(&a, "hello world\n").unwrap();
        let f2 = fingerprint(dir.path());
        assert_ne!(f1, f2, "nested edit must fire");

        // A git repo registers HEAD + the RESOLVED ref marker.
        std::fs::create_dir_all(dir.path().join(".git/refs/heads")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(dir.path().join(".git/refs/heads/main"), "abc123\n").unwrap();
        let f3 = fingerprint(dir.path());
        assert!(f3.contains_key("(git/HEAD)"));
        assert!(f3.contains_key("(git/index)"));
        assert!(f3.contains_key("(git/ref)"));

        // A COMMIT moves the branch ref → the fingerprint changes even though
        // HEAD's own content/mtime is untouched. (A longer hash makes the
        // size differ too — tmpfs mtime granularity is coarse.)
        std::fs::write(dir.path().join(".git/refs/heads/main"), "def4567890abc\n").unwrap();
        assert_ne!(f3, fingerprint(dir.path()), "commit (ref move) must fire");

        // New top-level file → different fingerprint.
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        assert_ne!(f3, fingerprint(dir.path()));
    }
}
