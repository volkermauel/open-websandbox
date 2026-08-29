//! Router construction. Split out of `main.rs` so the integration tests can
//! build an in-process app and exercise it via `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::execute::execute;
use crate::files::{
    archive, delete_entry, get_cwd, glob_search, grep, list_dir, list_ports, match_files, mkdir,
    move_entry, read_file, replace, search_files, serve_file, set_cwd, tool_download, tool_exists,
    tool_list, tool_list_root, tool_upload, upload, view_file, write_file,
};
use crate::snapshot::{restore, snapshot};
use crate::state::AppState;
use crate::terminals::{create_terminal, kill_terminal, list_terminals, terminal_get_or_ws};

/// Health/info payload returned by `GET /`. Field order is fixed
/// byte-for-byte (`status` first).
#[derive(serde::Serialize)]
struct Root {
    status: &'static str,
    runtime: &'static str,
}

async fn root() -> axum::Json<Root> {
    axum::Json(Root {
        status: "ok",
        runtime: "code-standard",
    })
}

async fn ok() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}

/// `GET /api/config` — feature discovery (open-terminal 0.8.1).
///
/// Unauthenticated exactly like upstream: it leaks only feature flags, and
/// inside the cluster the route is reachable solely through the broker
/// relay (broker auth still applies there). We always serve terminals and
/// implement neither notebooks nor a system-prompt endpoint.
async fn api_config() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "features": {
            "terminal": true,
            "notebooks": false,
            "system": false,
        }
    }))
}

/// Build the full runtime router over `state`.
pub fn build_router(state: AppState) -> Router {
    // #162: axum's DefaultBodyLimit (2 MiB) silently rejected uploads above
    // 2 MiB. Raise the request-body cap to the configured upload budget —
    // the workspace quota is still enforced at write time.
    let max_upload_bytes = state.config.max_upload_bytes as usize;
    Router::new()
        // Open (unauthenticated) routes: GET / + the two health probes.
        .route("/", get(root))
        .route("/healthz", get(ok))
        .route("/readyz", get(ok))
        // open-terminal 0.8.1 feature discovery — unauthenticated like upstream.
        .route("/api/config", get(api_config))
        // D9: Prometheus exposition (open — scraped without auth, matching the
        //      chart's PodMonitor on :8888/metrics).
        .route("/metrics", get(crate::metrics::metrics))
        // Gated routes: every handler declares `Authed` as its first extractor,
        // so each is individually fail-closed (a per-route auth
        // extractor).
        .route("/execute", post(execute))
        .route("/files/cwd", get(get_cwd).post(set_cwd))
        .route("/files/list", get(list_dir))
        .route("/files/read", get(read_file))
        .route("/files/write", post(write_file))
        .route("/files/mkdir", post(mkdir))
        .route("/files/move", post(move_entry))
        .route("/files/delete", delete(delete_entry))
        // PR-B-4 remaining file-operation surface (open-terminal + LLM-tool):
        .route("/files/view", get(view_file))
        // open-terminal 0.11.34: path-based inline serving for FileNav iframes.
        .route("/files/serve/{*file_path}", get(serve_file))
        // open-terminal 0.11.36 / 0.12.0: file-picker + unified search.
        .route("/files/search", get(search_files))
        .route("/files/matches", get(match_files))
        .route("/files/replace", post(replace))
        .route("/files/grep", get(grep))
        .route("/files/glob", get(glob_search))
        // PR-B-5: archive (zip) + multipart upload (open-terminal + LLM-tool).
        .route("/files/archive", post(archive))
        .route("/files/upload", post(upload))
        // LLM-tool surface (catch-all path params): download / list / exists.
        .route("/download/{*file_path}", get(tool_download))
        .route("/list", get(tool_list_root))
        .route("/list/", get(tool_list_root))
        .route("/list/{*file_path}", get(tool_list))
        .route("/exists/{*file_path}", get(tool_exists))
        // LLM-tool upload (multipart) — writes the file to the workspace base.
        .route("/upload", post(tool_upload))
        // Restricted runtime: no host-port introspection (empty ports list).
        .route("/ports", get(list_ports))
        // S3-tiered workspace offload/restore (#52): stream native tar+zstd.
        .route("/snapshot", get(snapshot))
        .route("/restore", put(restore))
        // Interactive PTY terminals (D5): HTTP CRUD + WebSocket relay.
        .route("/api/terminals", post(create_terminal).get(list_terminals))
        .route(
            "/api/terminals/{id}",
            get(terminal_get_or_ws).delete(kill_terminal),
        )
        // D9: record HTTP rate/latency for every served request, keyed by the
        //      templated route (bounded-cardinality `path` label).
        .layer(from_fn_with_state(
            state.clone(),
            crate::metrics::http_metrics_layer,
        ))
        .layer(DefaultBodyLimit::max(max_upload_bytes))
        .with_state(state)
}
