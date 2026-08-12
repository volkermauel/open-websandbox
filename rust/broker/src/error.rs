//! Error type mapping the canonical HTTP error contract onto axum responses.
//!
//! Mirrors [`runtime::error`](../../runtime/src/error.rs): every handler
//! failure surfaces as an [`ApiError`] that renders a JSON body of the shape
//! `{"detail": "..."}` with the same status codes, so the
//! HTTP surface is byte-for-byte compatible (D11). The broker-specific
//! [`ApiError::NotImplemented`] (501) marks the reverse-proxy surface that
//! PR-C-2 fills in.

#![forbid(unsafe_code)]

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// A handler error that maps onto the broker's HTTP status codes.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// 400 — malformed request body / missing required field.
    #[error("{0}")]
    BadRequest(String),
    /// 401 — missing or invalid shared Bearer.
    #[error("{0}")]
    Unauthorized(String),
    /// 404 — sandbox / template not found.
    #[error("{0}")]
    NotFound(String),
    /// 409 — the named object already exists and the caller did not request the
    /// fetch-existing path.
    #[error("{0}")]
    Conflict(String),
    /// 500 — internal failure.
    #[error("{0}")]
    Internal(String),
    /// 501 — broker-served surface that a later PR implements (resolve-on-
    /// request reverse proxy, terminal WS relay, metrics, `OpenAPI` gen).
    #[error("{0}")]
    NotImplemented(String),
    /// 502 — upstream Kubernetes apiserver rejected a sandbox lifecycle call.
    #[error("{0}")]
    BadGateway(String),
    /// 503 — shared secret not configured (fail-closed) / apiserver unreachable.
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
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            ApiError::BadGateway(_) => StatusCode::BAD_GATEWAY,
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
