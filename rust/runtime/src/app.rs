//! Router construction. Split out of `main.rs` so the integration tests can
//! build an in-process app and exercise it via `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::desktop::{
    desktop_audio_ws, desktop_browser_view, desktop_events_ws, desktop_fallback, desktop_stream_ws,
    merged_api_config,
};
use crate::execute::execute;
use crate::files::{
    archive, compare_files, delete_entry, display_file, get_cwd, glob_search, grep, list_dir,
    match_files, mkdir, move_entry, read_file, replace, search_files, serve_file, set_cwd,
    tool_download, tool_exists, tool_list, tool_list_root, tool_upload, upload, view_file,
    write_file,
};
use crate::ports::{list_ports, port_proxy, port_proxy_path};
use crate::skills::{list_skills, read_skill, read_skill_by_name};
use crate::snapshot::{restore, snapshot};
use crate::state::AppState;
use crate::system::{get_info, get_system};
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

/// The runtime's OWN `/api/config` feature flags — the base document the
/// desktop-sidecar merge (see [`crate::desktop::merged_api_config`]) builds on.
/// We always serve terminals, serve the system-prompt endpoint (stage 2, #169)
/// and implement neither notebooks.
fn runtime_config_json() -> serde_json::Value {
    serde_json::json!({
        "features": {
            "terminal": true,
            "notebooks": false,
            "system": true,
        }
    })
}

/// `GET /api/config` — feature discovery (open-terminal 0.8.1), merged with
/// the desktop sidecar's flags when the lane is active.
async fn api_config(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    axum::Json(merged_api_config(&state, runtime_config_json()).await)
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
        // Merged with the desktop sidecar's flags when DESKTOP_URL is set.
        .route("/api/config", get(api_config))
        // Desktop/browser sidecar lane (explicit WS relays — axum must own the
        // upgrade; see crate::desktop). Registered for the FULL method set (the
        // `/proxy/{port}` pattern) so non-WS/non-GET requests forward to the
        // sidecar exactly like the fallback instead of axum-405ing. Non-upgrade
        // requests on these paths are forwarded like the fallback, so the
        // explicit routes only ever EXTEND behaviour.
        .route(
            "/desktop/stream",
            get(desktop_stream_ws)
                .post(desktop_stream_ws)
                .put(desktop_stream_ws)
                .patch(desktop_stream_ws)
                .delete(desktop_stream_ws)
                .head(desktop_stream_ws)
                .options(desktop_stream_ws),
        )
        .route(
            "/desktop/audio",
            get(desktop_audio_ws)
                .post(desktop_audio_ws)
                .put(desktop_audio_ws)
                .patch(desktop_audio_ws)
                .delete(desktop_audio_ws)
                .head(desktop_audio_ws)
                .options(desktop_audio_ws),
        )
        .route(
            "/events",
            get(desktop_events_ws)
                .post(desktop_events_ws)
                .put(desktop_events_ws)
                .patch(desktop_events_ws)
                .delete(desktop_events_ws)
                .head(desktop_events_ws)
                .options(desktop_events_ws),
        )
        .route(
            "/browser/view/{sid}/{token}/{channel}",
            get(desktop_browser_view)
                .post(desktop_browser_view)
                .put(desktop_browser_view)
                .patch(desktop_browser_view)
                .delete(desktop_browser_view)
                .head(desktop_browser_view)
                .options(desktop_browser_view),
        )
        // OWUI v0.11.4 (#195): terminal-owned Agent Skills. `/{name}` is the
        // path-param alias OWUI's backend calls; `/skills/read?name=` is
        // upstream open-terminal's shape.
        .route("/skills", get(list_skills))
        .route("/skills/read", get(read_skill))
        .route("/skills/{name}", get(read_skill_by_name))
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
        // open-terminal 0.2.9: show-file signaling ({path, exists}).
        .route("/files/display", get(display_file))
        // open-terminal 0.11.36 / 0.12.0: file-picker + unified search.
        .route("/files/search", get(search_files))
        .route("/files/matches", get(match_files))
        .route("/files/replace", post(replace))
        .route("/files/grep", get(grep))
        .route("/files/glob", get(glob_search))
        // OWUI v0.11.4 (#195): read-only text comparison.
        .route("/files/compare", post(compare_files))
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
        // open-terminal 0.9.0 ports + 0.12.2 session-owned proxy (#169 stage 2):
        // both fed by the same descendant-owned visibility scan.
        .route("/ports", get(list_ports))
        // Exactly upstream's method set (others 405 like upstream).
        .route(
            "/proxy/{port}",
            get(port_proxy)
                .post(port_proxy)
                .put(port_proxy)
                .patch(port_proxy)
                .delete(port_proxy)
                .head(port_proxy)
                .options(port_proxy),
        )
        // Trailing-slash form ("/proxy/8080/"): upstream's `{path:path}`
        // captures an empty path here; axum needs the route spelled out.
        .route(
            "/proxy/{port}/",
            get(port_proxy)
                .post(port_proxy)
                .put(port_proxy)
                .patch(port_proxy)
                .delete(port_proxy)
                .head(port_proxy)
                .options(port_proxy),
        )
        .route(
            "/proxy/{port}/{*path}",
            get(port_proxy_path)
                .post(port_proxy_path)
                .put(port_proxy_path)
                .patch(port_proxy_path)
                .delete(port_proxy_path)
                .head(port_proxy_path)
                .options(port_proxy_path),
        )
        // open-terminal 0.11.27 / 0.11.6: LLM grounding + operator info.
        .route("/system", get(get_system))
        .route("/info", get(get_info))
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
        // Desktop/browser sidecar fallback: every request no explicit route
        // claims is reverse-proxied to DESKTOP_URL (HTTP-only; WS traffic uses
        // the explicit routes above). Inactive lane = the default 404.
        .fallback(desktop_fallback)
        .with_state(state)
}
