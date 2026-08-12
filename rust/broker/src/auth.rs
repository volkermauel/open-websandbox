//! Shared-Bearer authentication for Open WebUI ↔ broker requests.
//!
//! Shared-Bearer guard for every proxied / admin hop. Each request must
//! present `Authorization: Bearer <BROKER_SHARED_SECRET>`, compared constant-time
//! via [`shared::constant_time_eq`] so a mismatch reveals no timing signal.
//! Fail-closed contract (identical to the runtime's per-session guard):
//!
//! * an unset/placeholder `BROKER_SHARED_SECRET` is a misconfiguration → 503;
//! * a missing or mismatched Bearer → 401.
//!
//! Open probes (`/healthz`, `/readyz`, `/metrics`, `/openapi.json`) omit
//! [`Authed`] entirely, registered without an auth guard.

#![forbid(unsafe_code)]

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;

use shared::constant_time_eq;
use shared::is_placeholder_secret;

use crate::error::ApiError;
use crate::state::AppState;

use crate::metrics::AUTH_FAILURES_TOTAL;

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
/// simply omit it. It is the guard wired onto `/api/config`, `/api/status`,
/// and the catch-all proxy.
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
            metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => "misconfigured_secret")
                .increment(1);
            return Err(ApiError::ServiceUnavailable(
                "BROKER_SHARED_SECRET is not configured".to_string(),
            ));
        }
        match bearer_from_headers(&parts.headers) {
            Some(token) if constant_time_eq(&token, secret) => Ok(Authed),
            Some(_) => {
                // Present-but-mismatched Bearer: keep the message identical to the
                // missing-token case so the outcome label (not the body) is the only
                // signal — a brute-forcer learns nothing from the response text.
                metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => "bad_token").increment(1);
                Err(ApiError::Unauthorized("invalid bearer token".to_string()))
            }
            None => {
                metrics::counter!(AUTH_FAILURES_TOTAL, "outcome" => "missing_token").increment(1);
                Err(ApiError::Unauthorized("invalid bearer token".to_string()))
            }
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
