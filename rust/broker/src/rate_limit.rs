//! Per-user rate limiting (#98 A3).
//!
//! A `governor` token-bucket keyed by `X-User-Id` caps create / execute / file /
//! terminal traffic on the broker's gated routes. Open probes (`/healthz`,
//! `/readyz`, `/metrics`) are mounted on a separate, unlimited router in
//! [`crate::app::build_router`]. When a user's bucket is empty the request fails
//! fast with `429 Too Many Requests` (+ `Retry-After` / `x-ratelimit-*` headers)
//! instead of reaching the sandbox control plane — capping noisy-neighbour cost
//! and the brute-force/abuse surface from a single shared-secret caller.
//!
//! The limiter is intentionally *not* an auth gate: the key extractor never errors
//! (a request with no `X-User-Id` falls back to a shared `"anonymous"` bucket), so
//! authorization stays the `Authed` extractor's job. Note that, like most
//! pre-auth rate limiters, the bucket is keyed on the *claimed* `X-User-Id`; a
//! flood of unauthenticated requests asserting a victim's id could exhaust that
//! victim's bucket before auth rejects them — detection of that pattern is covered
//! by the `auth_failures_total` metrics + `OwuiBrokerAuthFailureRate` alert (#107).
#![forbid(unsafe_code)]

use axum::Router;
use tower_governor::{
    errors::GovernorError, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
    GovernorLayer,
};

use crate::state::AppState;

/// Key the token-bucket on the authenticated caller's `X-User-Id` (the OWUI user
/// the broker resolves sandboxes for). All callers share one `BROKER_SHARED_SECRET`,
/// so the user id is the only per-caller identity.
#[derive(Clone)]
pub(crate) struct XUserIdKey;

impl KeyExtractor for XUserIdKey {
    type Key = String;

    fn extract<T>(&self, req: &http::request::Request<T>) -> Result<Self::Key, GovernorError> {
        Ok(req
            .headers()
            .get("x-user-id")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| "anonymous".to_owned(), str::to_owned))
    }
}

/// Conditionally wrap `router` in the per-user governor layer. Returns it
/// unchanged when rate limiting is disabled (`broker.rateLimit.enabled = false`).
pub(crate) fn apply(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if !state.config.rate_limit_enabled {
        return router;
    }
    // governor rejects a zero refill/burst; clamp to 1 so a misconfigured 0 falls
    // back to the most permissive valid bucket rather than panicking at startup.
    let per_second = u64::from(state.config.rate_limit_per_second.max(1));
    let burst = state.config.rate_limit_burst.max(1);
    let conf = GovernorConfigBuilder::default()
        .per_second(per_second)
        .burst_size(burst)
        .key_extractor(XUserIdKey)
        .use_headers()
        .finish()
        .expect("governor config is well-formed for any positive per_second/burst");
    router.layer(GovernorLayer::new(conf))
}
