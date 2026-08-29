//! Error type mapping the canonical HTTP error contract onto axum responses.
//!
//! Every file/exec failure surfaces as an `ApiError` that renders a JSON body of
//! the canonical shape (`{"detail": "..."}`) with the same status codes, so the
//! HTTP surface is byte-for-byte compatible
//! (D11).

#![forbid(unsafe_code)]

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A handler error that maps 1:1 onto the runtime's HTTP status codes.
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
    /// 429 — `MAX_TERMINAL_SESSIONS` concurrent PTY sessions reached.
    #[error("{0}")]
    TooManyRequests(String),
    /// 413 — snapshot/restore stream exceeds `MAX_WORKSPACE_BYTES`.
    #[error("{0}")]
    PayloadTooLarge(String),
    /// 415 — non-image binary file on `/files/read` (open-terminal 0.2.7 parity).
    #[error("{0}")]
    UnsupportedMediaType(String),
    /// 502 — proxied upstream connection refused / transport failure
    /// (`/proxy/{port}`, upstream httpx `ConnectError` parity).
    #[error("{0}")]
    BadGateway(String),
    /// 504 — proxied upstream timeout (`/proxy/{port}`, upstream httpx
    /// `TimeoutException` parity).
    #[error("{0}")]
    GatewayTimeout(String),
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
            ApiError::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            ApiError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            ApiError::GatewayTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // Match the canonical error body shape exactly: {"detail": "..."}.
        let body = Json(json!({ "detail": self.to_string() }));
        (status, body).into_response()
    }
}

/// Convenience for the path-confinement guard.
pub(crate) const fn escapes() -> &'static str {
    "path escapes workspace"
}
