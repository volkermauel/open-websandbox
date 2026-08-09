//! Router construction. Split out of `main.rs` so the integration tests can
//! build an in-process app and exercise it via `tower::ServiceExt::oneshot`.

#![forbid(unsafe_code)]

use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::execute::execute;
use crate::files::{
    delete_entry, get_cwd, list_dir, mkdir, move_entry, read_file, set_cwd, write_file,
};
use crate::snapshot::{restore, snapshot};
use crate::state::AppState;

/// Health/info payload returned by `GET /`. Field order matches the Python
/// runtime byte-for-byte (`status` first).
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

/// Build the full runtime router over `state`.
pub fn build_router(state: AppState) -> Router {
    // Open (unauthenticated) routes: GET / + the two health probes.
    let open = Router::new()
        .route("/", get(root))
        .route("/healthz", get(ok))
        .route("/readyz", get(ok));

    // Gated routes: every handler declares `Authed` as its first extractor, so
    // each is individually fail-closed (mirrors Python's per-route Security dep).
    let gated = Router::new()
        .route("/execute", post(execute))
        .route("/files/cwd", get(get_cwd).post(set_cwd))
        .route("/files/list", get(list_dir))
        .route("/files/read", get(read_file))
        .route("/files/write", post(write_file))
        .route("/files/mkdir", post(mkdir))
        .route("/files/move", post(move_entry))
        .route("/files/delete", delete(delete_entry))
        // S3-tiered workspace offload/restore (#52): stream native tar+zstd.
        .route("/snapshot", get(snapshot))
        .route("/restore", put(restore));

    open.merge(gated.with_state(state))
}
