//! Shared-Bearer authentication for Open WebUI ↔ broker requests.
//!
//! Direct port of the Python broker's `_auth` dependency. Every proxied / admin
//! hop must present `Authorization: Bearer <BROKER_SHARED_SECRET>`, compared
//! constant-time via [`shared::constant_time_eq`] so a mismatch reveals no
//! timing signal. Fail-closed contract (identical to the runtime's per-session
//! guard):
//!
//! * an unset/placeholder `BROKER_SHARED_SECRET` is a misconfiguration → 503;
//! * a missing or mismatched Bearer → 401.
//!
//! Open probes (`/healthz`, `/readyz`, `/metrics`, `/openapi.json`) omit
//! [`Authed`] entirely, exactly as the Python routes register without
//! `Security(_auth)`.

#![forbid(unsafe_code)]

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;

use shared::constant_time_eq;
use shared::is_placeholder_secret;

use crate::error::ApiError;
use crate::state::AppState;

/// Extract a `Bearer <token>` value from request headers (`None` if absent or
/// not a Bearer scheme).
fn bearer_from_headers(headers: &HeaderMap) -> Option<Vec<u8>> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw.strip_prefix("Bearer ")?;
    Some(token.as_bytes().to_vec())
}

/// Extractor proof that a request passed the shared-Bearer guard.
///
/// Add this as the first handler parameter on every gated route; open routes
/// simply omit it. Mirrors the Python `Security(_auth)` dependency wired onto
/// `/api/config`, `/api/status`, and the catch-all proxy.
#[derive(Debug, Clone, Copy)]
pub struct Authed;

impl FromRequestParts<AppState> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let secret = state.config.shared_secret.as_bytes();
        if is_placeholder_secret(std::str::from_utf8(secret).unwrap_or("")) {
            // Misconfiguration, not "disabled": fail closed at the request path
            // regardless of the boot guard.
            return Err(ApiError::ServiceUnavailable(
                "BROKER_SHARED_SECRET is not configured".to_string(),
            ));
        }
        match bearer_from_headers(&parts.headers) {
            Some(token) if constant_time_eq(&token, secret) => Ok(Authed),
            _ => Err(ApiError::Unauthorized("invalid bearer token".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer_from_headers(&h), None);
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer abc".parse().unwrap(),
        );
        assert_eq!(bearer_from_headers(&h), Some(b"abc".to_vec()));
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic xyz".parse().unwrap(),
        );
        assert_eq!(bearer_from_headers(&h), None);
    }
}
