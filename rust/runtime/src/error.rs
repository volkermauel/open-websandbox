//! Error type mapping the FastAPI `HTTPException` contract onto axum responses.
//!
//! Every file/exec failure surfaces as an `ApiError` that renders a JSON body of
//! the shape FastAPI produces (`{"detail": "..."}`) with the same status codes
//! the Python runtime returns, so the HTTP surface is byte-for-byte compatible
//! (D11).

#![forbid(unsafe_code)]

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A handler error that maps 1:1 onto the Python runtime's HTTP status codes.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400 — bad request: path traversal/escape, invalid subdir, write failure.
    #[error("{0}")]
    BadRequest(String),
    /// 401 — missing or invalid per-session Bearer.
    #[error("{0}")]
    Unauthorized(String),
    /// 404 — file/directory/path not found.
    #[error("{0}")]
    NotFound(String),
    /// 409 — move destination already exists.
    #[error("{0}")]
    Conflict(String),
    /// 413 — snapshot/restore stream exceeds `MAX_WORKSPACE_BYTES`.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// 500 — internal filesystem/list failure.
    #[error("{0}")]
    Internal(String),
    /// 503 — per-session API key not configured (fail-closed).
    #[error("{0}")]
    ServiceUnavailable(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Match FastAPI's HTTPException body shape exactly: {"detail": "..."}.
        let body = Json(json!({ "detail": self.to_string() }));
        (status, body).into_response()
    }
}

/// Convenience for the path-confinement guard.
pub(crate) const fn escapes() -> &'static str {
    "path escapes workspace"
}
