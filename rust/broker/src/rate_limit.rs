//! Per-chat + per-user rate limiting (#161; per-user originally #98 A3).
//!
//! Two stacked `governor` token-buckets cap create / execute / file / terminal
//! traffic on the broker's gated routes:
//!
//! * **inner (per chat)** — keyed on `X-User-Id` + `X-Session-Id`, refilling at
//!   `BROKER_RATE_LIMIT_PER_SECOND` / `BROKER_RATE_LIMIT_BURST` (default 60/120).
//!   One chat's FileNav polling can no longer starve the user's other chats.
//! * **outer (per user)** — keyed on `X-User-Id` alone, with the per-chat budget
//!   scaled by `BROKER_RATE_LIMIT_USER_MULTIPLIER` (default 5 → 300/600). A
//!   user's aggregate stays bounded, preserving the #98 noisy-neighbour cap.
//!
//! Open probes (`/healthz`, `/readyz`, `/metrics`) are mounted on a separate,
//! unlimited router in [`crate::app::build_router`]. When a bucket is empty the
//! request fails fast with `429 Too Many Requests` instead of reaching the
//! sandbox control plane. Only the per-chat layer writes `Retry-After` /
//! `x-ratelimit-*` headers: tower_governor layers overwrite each other's
//! headers on the way out, so a single header set can only describe one layer.
//! A 429 WITH headers therefore means that chat's bucket is empty; a 429
//! WITHOUT them means the per-user aggregate tripped first.
//!
//! The limiter is intentionally *not* an auth gate: neither key extractor errors
//! (a request with no `X-User-Id` falls back to a shared `"anonymous"` bucket),
//! so authorization stays the `Authed` extractor's job. Note that, like most
//! pre-auth rate limiters, the buckets key on the *claimed* `X-User-Id`; a
//! flood of unauthenticated requests asserting a victim's id could exhaust that
//! victim's buckets before auth rejects them — detection of that pattern is
//! covered by the `auth_failures_total` metrics + `OwuiBrokerAuthFailureRate`
//! alert (#107).
#![forbid(unsafe_code)]

use axum::Router;
use tower_governor::{
    errors::GovernorError, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
    GovernorLayer,
};

use crate::state::AppState;

fn header(req_headers: &http::HeaderMap, name: &str) -> Option<String> {
    req_headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Key the outer token-bucket on the caller's `X-User-Id` (the OWUI user the
/// broker resolves sandboxes for). All callers share one `BROKER_SHARED_SECRET`,
/// so the user id is the only per-caller identity.
#[derive(Clone)]
pub(crate) struct XUserIdKey;

impl KeyExtractor for XUserIdKey {
    type Key = String;

    fn extract<T>(&self, req: &http::request::Request<T>) -> Result<Self::Key, GovernorError> {
        Ok(header(req.headers(), "x-user-id").unwrap_or_else(|| "anonymous".to_owned()))
    }
}

/// Key the inner token-bucket on `X-User-Id` + `X-Session-Id` — the chat the
/// traffic belongs to. `X-Session-Id` defaults to the user id when OWUI omits
/// it (same fallback as [`crate::proxy`]), so id-less traffic lands in the
/// user's draft "chat" bucket rather than a shared one.
#[derive(Clone)]
pub(crate) struct UserSessionKey;

/// Separator for the composite key. `U+001F` (unit separator) cannot appear in
/// header values (HTTP field values exclude control chars), so distinct
/// user/session pairs cannot collide.
const KEY_SEP: char = '\u{1f}';

impl KeyExtractor for UserSessionKey {
    type Key = String;

    fn extract<T>(&self, req: &http::request::Request<T>) -> Result<Self::Key, GovernorError> {
        let user = header(req.headers(), "x-user-id").unwrap_or_else(|| "anonymous".to_owned());
        let session = header(req.headers(), "x-session-id").unwrap_or_else(|| user.clone());
        Ok(format!("{user}{KEY_SEP}{session}"))
    }
}

/// Conditionally wrap `router` in the governor layers. Returns it unchanged
/// when rate limiting is disabled (`broker.rateLimit.enabled = false`).
pub(crate) fn apply(router: Router<AppState>, state: &AppState) -> Router<AppState> {
    if !state.config.rate_limit_enabled {
        return router;
    }
    // governor rejects a zero refill/burst; clamp to 1 so a misconfigured 0 falls
    // back to the most permissive valid bucket rather than panicking at startup.
    let per_second = u64::from(state.config.rate_limit_per_second.max(1));
    let burst = state.config.rate_limit_burst.max(1);
    // Per-user aggregate = per-chat budget x multiplier (#161). Multiplier 0 is
    // meaningless (an aggregate tighter than each chat) — clamp to 1. Products
    // saturate rather than panic on absurd configs.
    let multiplier = state.config.rate_limit_user_multiplier.max(1);
    let chat_conf = GovernorConfigBuilder::default()
        .per_second(per_second)
        .burst_size(burst)
        .key_extractor(UserSessionKey)
        .use_headers()
        .finish()
        .expect("governor config is well-formed for any positive per_second/burst");
    // Headerless on purpose (see module docs): headers on the inner (chat)
    // layer only, so a 429's headers always describe the chat bucket and a
    // headerless 429 identifies the user aggregate.
    let user_conf = GovernorConfigBuilder::default()
        .per_second(per_second * u64::from(multiplier))
        .burst_size(burst.saturating_mul(multiplier))
        .key_extractor(XUserIdKey)
        .finish()
        .expect("governor config is well-formed for any positive per_second/burst");
    // Layers compose: the per-user layer is outer (first to reject), the
    // per-chat layer inner (fairness between one user's chats).
    router
        .layer(GovernorLayer::new(chat_conf))
        .layer(GovernorLayer::new(user_conf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(user: Option<&str>, session: Option<&str>) -> http::Request<()> {
        let mut builder = http::Request::builder().uri("/execute");
        if let Some(u) = user {
            builder = builder.header("x-user-id", u);
        }
        if let Some(s) = session {
            builder = builder.header("x-session-id", s);
        }
        builder.body(()).expect("test request")
    }

    #[test]
    fn user_key_falls_back_to_anonymous() {
        assert_eq!(XUserIdKey.extract(&req(None, None)).unwrap(), "anonymous");
        assert_eq!(XUserIdKey.extract(&req(Some(" u1 "), None)).unwrap(), "u1");
    }

    #[test]
    fn session_key_composes_user_and_session() {
        let key = UserSessionKey
            .extract(&req(Some("u1"), Some("s1")))
            .unwrap();
        assert_eq!(key, format!("u1{KEY_SEP}s1"));
    }

    #[test]
    fn session_key_defaults_session_to_user() {
        // No X-Session-Id: id-less traffic shares the user's draft bucket.
        let key = UserSessionKey.extract(&req(Some("u1"), None)).unwrap();
        assert_eq!(key, format!("u1{KEY_SEP}u1"));
    }

    #[test]
    fn session_key_falls_back_to_anonymous() {
        let key = UserSessionKey.extract(&req(None, Some("s1"))).unwrap();
        assert_eq!(key, format!("anonymous{KEY_SEP}s1"));
    }

    #[test]
    fn separator_cannot_be_injected_via_headers() {
        // The composite key is injective because the separator (U+001F) can
        // never appear in a header value: http rejects control chars outright,
        // so no (user, session) pair can forge another pair's key.
        assert!(http::HeaderValue::from_str("u1\u{1f}s1").is_err());
        let a = UserSessionKey
            .extract(&req(Some("u1"), Some("s1")))
            .unwrap();
        let b = UserSessionKey
            .extract(&req(Some("u1"), Some("s1")))
            .unwrap();
        assert_eq!(a, b);
        let c = UserSessionKey
            .extract(&req(Some("u1"), Some("s2")))
            .unwrap();
        assert_ne!(a, c);
    }
}
