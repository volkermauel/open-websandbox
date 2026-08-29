//! Port visibility + the session-owned reverse proxy (`GET /ports`,
//! `/proxy/{port}`) — open-terminal 0.9.0 surface with the **0.12.2
//! ownership lockdown**.
//!
//! Upstream ownership semantics (open_terminal/main.py `_visible_ports`,
//! single-user mode): a port is visible iff its listening socket belongs to a
//! *descendant process of the server*. We mirror that exactly against
//! `/proc`: parse `/proc/net/tcp{,6}` for `LISTEN` rows, resolve each socket
//! inode to a pid via `/proc/<pid>/fd/*` readlinks, and keep sockets owned by
//! descendants of the runtime pid. `/execute` children and PTY shells are
//! runtime children, so services the session starts are owned; orphans
//! reparent to PID 1 — which in the sandbox *is* the runtime — so they stay
//! owned. The runtime's own `:8888` listener is **not** a descendant of
//! itself and is therefore excluded, exactly like upstream. The upstream
//! multi-user UID branch does not apply (one OS user per sandbox — documented
//! divergence in docs/compatibility.md).
//!
//! The proxy forwards method + path + query + buffered body to
//! `http://localhost:{port}/{path}`, stripping hop-by-hop headers and the
//! inbound `Authorization` (upstream strips the API key — the proxied app
//! never sees it). Transport failures map to upstream's exact strings:
//! 502 `Connection refused: localhost:{port}` / 504 `Timeout connecting to
//! localhost:{port}`. Response headers keep `content-encoding` (our client
//! does not decompress, so bytes and headers stay self-consistent — upstream
//! strips it because httpx already decoded the body; delivery-equivalent).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Json;
use serde::Serialize;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Request headers never forwarded upstream (upstream's strip set + host;
/// `content-length` is dropped so the client recomputes it from the body).
const REQUEST_STRIP: &[&str] = &["host", "transfer-encoding", "connection", "authorization"];

/// Response headers not copied back to the client (`content-length` is
/// rederived by axum from the buffered bytes).
const RESPONSE_STRIP: &[&str] = &["transfer-encoding", "connection", "content-length"];

/// One visible listening port, as reported by `GET /ports` (upstream strips
/// the internal `uid` field; `pid`/`process` are `null` when unresolvable).
#[derive(Debug, Serialize)]
pub struct VisiblePort {
    /// TCP port number.
    port: u16,
    /// Owning process id (never `None` for visible ports — unresolvable
    /// sockets are excluded, mirroring upstream's descendant filter).
    pid: Option<u32>,
    /// `/proc/<pid>/comm` of the owning process.
    process: Option<String>,
}

/// `GET /ports` — the ports the proxy would allow (upstream `_visible_ports`
/// feeds both endpoints; ours does too).
#[utoipa::path(
    get,
    path = "/ports",
    tag = "ports",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Session-visible listening ports (descendant-owned)", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn list_ports(_auth: Authed) -> Json<serde_json::Value> {
    let ports = visible_ports().await;
    Json(serde_json::json!({ "ports": ports }))
}

/// `POST|GET|PUT|PATCH|DELETE|HEAD|OPTIONS /proxy/{port}` — proxy the root
/// path of an owned port (upstream `/proxy/{port}/{path:path}` with an empty
/// path).
///
/// # Errors
///
/// Returns [`ApiError`] per the upstream contract: 400 for an out-of-range
/// port, 404 `Port not found` for unowned ports, 502/504 on upstream
/// connect/timeout failures.
pub async fn port_proxy(
    _auth: Authed,
    State(state): State<AppState>,
    Path(port): Path<u16>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    proxy_request(state, port, "", query, method, headers, body).await
}

/// `POST|GET|PUT|PATCH|DELETE|HEAD|OPTIONS /proxy/{port}/{*path}` — proxy a
/// sub-path of an owned port.
///
/// # Errors
///
/// As [`port_proxy`].
pub async fn port_proxy_path(
    _auth: Authed,
    State(state): State<AppState>,
    Path((port, path)): Path<(u16, String)>,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    proxy_request(state, port, &path, query, method, headers, body).await
}

/// Core proxy: ownership check, forward, response rebuild
/// (upstream `port_proxy`, open_terminal/main.py @ v0.12.3).
async fn proxy_request(
    state: AppState,
    port: u16,
    path: &str,
    query: Option<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if port < 1 {
        // Upstream 422; axum surfaces 400 — documented divergence.
        return Err(ApiError::BadRequest(
            "Port must be between 1 and 65535".to_string(),
        ));
    }
    let owned = visible_ports().await.iter().any(|p| p.port == port);
    if !owned {
        return Err(ApiError::NotFound("Port not found".to_string()));
    }

    let mut url = format!("http://localhost:{port}/{path}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(&q);
    }

    let mut forwarded = HeaderMap::new();
    for (name, value) in &headers {
        if REQUEST_STRIP.contains(&name.as_str()) || name.as_str() == "content-length" {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }

    let request = state.proxy_client.request(method, &url).headers(forwarded);
    let request = if body.is_empty() {
        request
    } else {
        request.body(body)
    };

    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return Err(ApiError::GatewayTimeout(format!(
                "Timeout connecting to localhost:{port}"
            )));
        }
        Err(_) => {
            return Err(ApiError::BadGateway(format!(
                "Connection refused: localhost:{port}"
            )));
        }
    };

    let status = upstream.status();
    let response_headers = upstream.headers().clone();
    let bytes = upstream
        .bytes()
        .await
        .map_err(|_| ApiError::BadGateway(format!("Connection refused: localhost:{port}")))?;

    // reqwest's `StatusCode` *is* `http::StatusCode` (shared http-1.x tree).
    let mut builder = Response::builder().status(status);
    for (name, value) in &response_headers {
        if RESPONSE_STRIP.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(bytes))
        .map_err(|e| ApiError::Internal(format!("proxy response build failed: {e}")))
}

/// Scan the session-visible ports (descendant-owned localhost listeners).
async fn visible_ports() -> Vec<VisiblePort> {
    tokio::task::spawn_blocking(scan_visible_ports)
        .await
        .unwrap_or_default()
}

/// Blocking scan core (upstream `detect_listening_ports` + descendant filter,
/// open_terminal/utils/port.py).
fn scan_visible_ports() -> Vec<VisiblePort> {
    let descendants = descendant_pids(std::process::id());
    let listening = listening_rows();
    let wanted: HashSet<u64> = listening.iter().map(|row| row.inode).collect();
    let socket_pids = socket_pids(&wanted);

    let mut visible: Vec<VisiblePort> = listening
        .into_iter()
        .filter_map(|row| {
            let pid = socket_pids.get(&row.inode).copied()?;
            descendants.contains(&pid).then(|| VisiblePort {
                port: row.port,
                pid: Some(pid),
                process: process_name(pid),
            })
        })
        .collect();
    visible.sort_by_key(|p| p.port);
    visible
}

/// One `LISTEN` row of `/proc/net/tcp[6]`.
struct ListenRow {
    port: u16,
    #[expect(
        dead_code,
        reason = "kept for parity with upstream's row shape; uid is filtered out of the response"
    )]
    uid: u32,
    inode: u64,
}

/// Parse `/proc/net/tcp` + `/proc/net/tcp6` for `LISTEN` sockets (state
/// `0A`), deduplicated by port (first row wins, like upstream).
fn listening_rows() -> Vec<ListenRow> {
    let mut ports: HashMap<u16, ListenRow> = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 10 || parts[3] != "0A" {
                continue;
            }
            let Some(hex_port) = parts[1].rsplit(':').next() else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(hex_port, 16) else {
                continue;
            };
            if port == 0 {
                continue;
            }
            let (Ok(uid), Ok(inode)) = (parts[7].parse::<u32>(), parts[9].parse::<u64>()) else {
                continue;
            };
            ports.entry(port).or_insert(ListenRow { port, uid, inode });
        }
    }
    ports.into_values().collect()
}

/// Resolve the wanted socket inodes to owning pids by scanning
/// `/proc/<pid>/fd/*` readlinks (`socket:[inode]`).
fn socket_pids(wanted: &HashSet<u64>) -> HashMap<u64, u32> {
    let mut map = HashMap::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in proc_dir.flatten() {
        let Some(pid) = file_name_u32(&entry) else {
            continue;
        };
        let Ok(fd_dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fd_dir.flatten() {
            let Ok(link) = fd.path().read_link() else {
                continue;
            };
            let Some(inode) = link
                .to_str()
                .and_then(|s| s.strip_prefix("socket:["))
                .and_then(|s| s.strip_suffix(']'))
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            if wanted.contains(&inode) {
                map.entry(inode).or_insert(pid);
            }
        }
    }
    map
}

/// All pids that are descendants of `root` (exclusive) — BFS over the
/// parent→children map built from `/proc/<pid>/stat`.
fn descendant_pids(root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    for entry in proc_dir.flatten() {
        let Some(pid) = file_name_u32(&entry) else {
            continue;
        };
        let Some(ppid) = stat_ppid(pid) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }

    let mut descendants = HashSet::new();
    let mut queue: Vec<u32> = children.get(&root).cloned().unwrap_or_default();
    while let Some(pid) = queue.pop() {
        if descendants.insert(pid) {
            queue.extend(children.get(&pid).cloned().unwrap_or_default());
        }
    }
    descendants
}

/// `PPid` from `/proc/<pid>/stat`, parsed robustly (everything after the
/// final `)`; `ppid` is the second field of that remainder — the naive
/// whitespace split upstream uses breaks on comms containing spaces).
fn stat_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// `/proc/<pid>/comm` (kernel-trimmed to 15 chars), first line.
fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    comm.lines().next().map(str::to_string)
}

/// A numeric `/proc` entry name as a pid.
fn file_name_u32(entry: &std::fs::DirEntry) -> Option<u32> {
    entry.file_name().to_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime's own process is never its own descendant (upstream's
    /// exclusive-descendants rule ⇒ the server's own listener is invisible).
    #[test]
    fn descendants_exclude_self() {
        let d = descendant_pids(std::process::id());
        assert!(!d.contains(&std::process::id()));
    }

    /// Listening rows parse: this test process itself typically holds at
    /// least one socket; at minimum the parser must not see bogus ports.
    #[test]
    fn listening_rows_are_well_formed() {
        for row in listening_rows() {
            assert!((1..=65535).contains(&row.port));
        }
    }
}
