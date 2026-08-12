//! Reverse proxy: forward Open WebUI runtime-tool requests to the resolved
//! sandbox pod.
//!
//! The shared Bearer is validated up-front by
//! [`Authed`](crate::auth::Authed); here we:
//!
//! 1. read the OWUI request identity (`X-User-Id` required; `X-Session-Id`
//!    defaults to the user; `X-Persistence` selects the profile, else the
//!    configured default);
//! 2. [`resolve_sandbox`] for that identity (get-or-create + Ready poll);
//! 3. rebuild the headers: strip the hop-by-hop + broker-managed set
//!    ([`HOP`], including the inbound `Authorization`), then inject the runtime
//!    Bearer (`BROKER_RUNTIME_API_KEY` — C-2 shared key; C-3 rotates per session)
//!    and the `X-Sandbox-*` identity the runtime echoes;
//! 4. forward method + path + query + body to `http://<pod-ip>:8888<path>` over
//!    the shared reqwest client and stream the response (status + body + headers)
//!    back, rewriting 3xx `Location` so redirects stay broker-relative.
//!
//! The outbound WS terminal relay lives in [`crate::terminal`]; everything else
//! (`/execute`, `/files/*`, `/snapshot`, `/restore`, `POST /api/terminals`, …)
//! flows through here.

#![forbid(unsafe_code)]

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request};
use axum::response::Response;

use shared::Profile;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::metrics::RUNTIME_HOP_ERRORS_TOTAL;
use crate::resolve::{resolve_sandbox, ResolvedSandbox};
use crate::state::AppState;
use tracing::Instrument;

/// Hop-by-hop + broker-managed headers stripped before forwarding ([`HOP`]).
///
/// `authorization` is dropped here so the inbound OWUI→broker Bearer never reaches
/// the runtime; the runtime hop gets its OWN Bearer (the runtime key) injected
/// below. `content-length`/`host`/`transfer-encoding` are recomputed by reqwest.
const HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "authorization",
];

/// Runtime pod port (hard-coded `:8888`).
pub const RUNTIME_PORT: u16 = 8888;

/// Cap on a forwarded request body read into memory (256 MiB). This handler
/// also buffers the full body; streaming the request body
/// end-to-end is a later optimisation. 256 MiB covers realistic `/files` uploads.
const MAX_FORWARD_BODY: usize = 256 * 1024 * 1024;

/// Identity the broker derives from the inbound request (OWUI → broker hop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardIdentity {
    /// `X-User-Id` (required).
    pub user_id: String,
    /// `X-Session-Id`, defaulting to the user id when OWUI omits it.
    pub session_id: String,
    /// Profile from `X-Persistence` (`persistent`/`ephemeral`), else the default.
    pub profile: Profile,
}

/// Parse the OWUI request identity from the inbound headers.
///
/// `X-User-Id` is required (400 when absent); `X-Session-Id` defaults to the user
/// id; an unrecognised/absent `X-Persistence` selects the configured default
/// profile (`BROKER_DEFAULT_PROFILE`).
pub fn forward_identity(
    headers: &HeaderMap,
    default_profile: Profile,
) -> Result<ForwardIdentity, ApiError> {
    let user_id = headers
        .get("x-user-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if user_id.is_empty() {
        return Err(ApiError::BadRequest(
            "X-User-Id header is required".to_string(),
        ));
    }
    let session_id = headers
        .get("x-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| user_id.to_string());
    let profile = profile_from_header(headers, default_profile);
    Ok(ForwardIdentity {
        user_id: user_id.to_string(),
        session_id,
        profile,
    })
}

/// Resolve the persistence profile from `X-Persistence`, else the default.
pub(crate) fn profile_from_header(headers: &HeaderMap, default_profile: Profile) -> Profile {
    match headers
        .get("x-persistence")
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("persistent") => Profile::Persistent,
        Some("ephemeral") => Profile::Ephemeral,
        _ => default_profile,
    }
}

/// Build the header map forwarded to the runtime pod.
///
/// Copies every inbound header whose lowercased name is NOT in [`HOP`], then
/// injects the runtime Bearer (when a runtime key is configured) and the
/// `X-Sandbox-*` / `X-Session-Id` identity the runtime echoes back.
#[must_use]
pub fn build_forward_headers(
    inbound: &HeaderMap,
    resolved: &ResolvedSandbox,
    runtime_ns: &str,
    session_id: &str,
    runtime_api_key: &str,
) -> HeaderMap {
    let mut fwd = HeaderMap::new();
    for (name, value) in inbound.iter() {
        if is_hop(name.as_str()) {
            continue;
        }
        fwd.append(name.clone(), value.clone());
    }
    insert_unique(&mut fwd, "x-sandbox-id", &resolved.name);
    insert_unique(&mut fwd, "x-sandbox-namespace", runtime_ns);
    insert_unique(&mut fwd, "x-sandbox-pod-ip", &resolved.pod_ip);
    insert_unique(&mut fwd, "x-session-id", session_id);
    if !runtime_api_key.is_empty() {
        // The direct broker→runtime pod credential. C-2: single shared key from
        // BrokerConfig; C-3 resolves this pod's per-session Secret instead.
        fwd.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {runtime_api_key}"))
                .expect("runtime api key is a valid header value"),
        );
    }
    fwd
}

/// True when `name` (case-insensitive) is a hop-by-hop / broker-managed header.
fn is_hop(name: &str) -> bool {
    HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Overwrite `name` with `value` (single-valued broker-injected headers).
fn insert_unique(headers: &mut HeaderMap, name: &str, value: &str) {
    let name = name.parse::<HeaderName>().expect("static header name");
    let value = HeaderValue::from_str(value).expect("sandbox identity is a valid header value");
    headers.insert(name, value);
}

/// Upstream URL the broker forwards to.
///
/// `http://<pod-ip>:8888<path>` (path includes the leading `/` and any query),
/// unless a test/dev override base is set on [`AppState`] — then
/// `{override}{path}` (used by the in-process proxy tests to point at a mock).
#[must_use]
pub fn upstream_url(state: &AppState, resolved: &ResolvedSandbox, path_and_query: &str) -> String {
    if let Some(base) = state.runtime_upstream_override.as_deref() {
        format!("{base}{path_and_query}")
    } else {
        format!(
            "http://{}:{}{path_and_query}",
            resolved.pod_ip, RUNTIME_PORT
        )
    }
}

/// Strip the host from a 3xx `Location` so redirects stay broker-relative
/// (keep only the path + query). A relative `Location` is
/// returned unchanged.
#[must_use]
pub fn rewrite_location(loc: &str) -> String {
    // Drop scheme + netloc,
    // keep the path (+ query), leave a relative path verbatim. A fragment (if
    // any) is dropped to match urlsplit's separate fragment slot.
    let after_authority = if let Some((_, rest)) = loc.split_once("://") {
        // `rest` is `<authority>/path?query`; the path starts at the first '/'.
        rest.find('/').map_or("", |idx| &rest[idx..])
    } else if let Some(rest) = loc.strip_prefix("//") {
        // scheme-relative `//authority/path`
        rest.find('/').map_or("", |idx| &rest[idx..])
    } else {
        loc // no authority → relative path kept as-is
    };
    after_authority
        .split_once('#')
        .map(|(p, _)| p)
        .unwrap_or(after_authority)
        .to_string()
}

/// `/{*path}` catch-all reverse proxy. Resolves the target sandbox, then forwards
/// the request to the runtime pod and streams the response back.
pub async fn proxy_catch_all(
    _: Authed,
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response, ApiError> {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| "/".to_string());

    let identity = forward_identity(&headers, state.config.default_profile)?;
    let resolved = resolve_sandbox(
        &state,
        &identity.user_id,
        &identity.session_id,
        identity.profile,
    )
    .await?;

    // #98: the per-session runtime key is MANDATORY (hard cutover, no shared-key
    // fallback). A missing or unreadable key is a misconfiguration — reject the
    // request rather than fall back to a shared `BROKER_RUNTIME_API_KEY`, which would
    // re-open the cross-sandbox lateral-movement risk per-session keys eliminated.
    let runtime_api_key = match state.store.read_runtime_key(&resolved.name).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Err(ApiError::BadGateway(format!(
                "no per-session runtime key for sandbox {} (shared-key fallback removed; \
                 ensure the broker provisioned an owui-runtime-key-* secret)",
                resolved.name
            )));
        }
        Err(e) => {
            tracing::warn!(sandbox = %resolved.name, error = %e, "read runtime key failed");
            return Err(ApiError::BadGateway(format!(
                "read runtime key for sandbox {} failed: {e}",
                resolved.name
            )));
        }
    };
    let fwd = build_forward_headers(
        &headers,
        &resolved,
        &state.config.runtime_ns,
        &identity.session_id,
        &runtime_api_key,
    );

    let body = to_bytes(req.into_body(), MAX_FORWARD_BODY)
        .await
        .map_err(|e| ApiError::BadRequest(format!("could not read request body: {e}")))?;
    let url = upstream_url(&state, &resolved, &path_and_query);

    let upstream = state
        .http
        .request(method, &url)
        .headers(fwd)
        .body(body)
        .send()
        .instrument(tracing::info_span!(
            "runtime.hop",
            "runtime.pod" = %resolved.pod_ip,
            "http.url" = %url,
        ))
        .await
        .map_err(|e| {
            // D9: a runtime hop transport/connect/send failure.
            metrics::counter!(RUNTIME_HOP_ERRORS_TOTAL).increment(1);
            ApiError::BadGateway(format!("runtime hop to {url} failed: {e}"))
        })?;

    forward_response(upstream).await
}

/// Stream an upstream reqwest response back as an axum response: copy status +
/// non-hop headers, rewrite a 3xx `Location`, and stream the body.
async fn forward_response(upstream: reqwest::Response) -> Result<Response, ApiError> {
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    let mut out = HeaderMap::new();
    for (name, value) in &upstream_headers {
        if is_hop(name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    // Keep redirects broker-relative (the runtime's pod IP is unreachable outside
    // the cluster — e.g. Starlette's /list/. → /list/ 307).
    if status.is_redirection() {
        if let Some(loc) = upstream_headers
            .get("location")
            .and_then(|v| v.to_str().ok())
        {
            let rewritten = rewrite_location(loc);
            if let Ok(value) = HeaderValue::from_str(&rewritten) {
                out.insert("location", value);
            }
        }
    }

    let mut builder = Response::builder().status(status);
    for (name, value) in &out {
        builder = builder.header(name.clone(), value.clone());
    }
    // Stream the upstream body straight through; streaming is strictly better
    // and avoids buffering large /files payloads in the broker.
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| ApiError::Internal(format!("could not build proxy response: {e}")))
}

/// Convenience used by tests/probes to assert the hop-by-hop set contents.
#[cfg(test)]
fn hop_set() -> &'static [&'static str] {
    HOP
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn resolved() -> ResolvedSandbox {
        ResolvedSandbox {
            name: "owui-c-deadbeef".into(),
            pod_ip: "10.0.0.5".into(),
        }
    }

    fn headers_with(
        user: Option<&str>,
        session: Option<&str>,
        persist: Option<&str>,
        bearer: Option<&str>,
    ) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(u) = user {
            h.insert("x-user-id", HeaderValue::from_str(u).unwrap());
        }
        if let Some(s) = session {
            h.insert("x-session-id", HeaderValue::from_str(s).unwrap());
        }
        if let Some(p) = persist {
            h.insert("x-persistence", HeaderValue::from_str(p).unwrap());
        }
        if let Some(b) = bearer {
            h.insert(
                "authorization",
                HeaderValue::from_str(&format!("Bearer {b}")).unwrap(),
            );
        }
        h
    }

    // --- forward_identity ---------------------------------------------------

    #[test]
    fn identity_requires_user_id() {
        let err = forward_identity(&headers_with(None, None, None, None), Profile::Persistent)
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn identity_session_defaults_to_user() {
        let id = forward_identity(
            &headers_with(Some("u1"), None, None, None),
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.user_id, "u1");
        assert_eq!(id.session_id, "u1");
        assert_eq!(id.profile, Profile::Persistent);
    }

    #[test]
    fn identity_reads_explicit_session_and_profile() {
        let id = forward_identity(
            &headers_with(Some("u1"), Some("chat-9"), Some("ephemeral"), None),
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.user_id, "u1");
        assert_eq!(id.session_id, "chat-9");
        assert_eq!(id.profile, Profile::Ephemeral);
    }

    #[test]
    fn identity_falls_back_to_default_profile_on_unknown_persistence() {
        let id = forward_identity(
            &headers_with(Some("u1"), None, Some("bogus"), None),
            Profile::Ephemeral,
        )
        .unwrap();
        assert_eq!(id.profile, Profile::Ephemeral);
    }

    #[test]
    fn identity_treats_empty_session_as_absent() {
        let id = forward_identity(
            &headers_with(Some("u1"), Some(""), None, None),
            Profile::Persistent,
        )
        .unwrap();
        assert_eq!(id.session_id, "u1");
    }

    // --- build_forward_headers ---------------------------------------------

    #[test]
    fn forward_headers_strip_hop_and_inbound_authorization() {
        let h = headers_with(Some("u"), Some("s"), None, Some("owui-bearer"));
        // With a runtime key set, the inbound OWUI Bearer is REPLACED by the
        // runtime Bearer (asserted in the dedicated test below); here we only
        // assert the other hop-by-hop headers are gone.
        let fwd = build_forward_headers(&h, &resolved(), "agent-sandbox-runtime", "s", "rt-key");
        assert!(fwd.get("host").is_none());
        assert!(fwd.get("content-length").is_none());
        assert!(fwd.get("transfer-encoding").is_none());
        assert!(fwd.get("upgrade").is_none());
    }

    #[test]
    fn forward_headers_inject_runtime_bearer_and_sandbox_identity() {
        let h = headers_with(Some("u"), Some("s"), None, Some("owui-bearer"));
        let fwd = build_forward_headers(
            &h,
            &resolved(),
            "agent-sandbox-runtime",
            "chat-s",
            "rt-secret",
        );
        assert_eq!(
            fwd.get("authorization").unwrap(),
            "Bearer rt-secret",
            "runtime hop carries the runtime key, not the OWUI bearer"
        );
        assert_eq!(fwd.get("x-sandbox-id").unwrap(), "owui-c-deadbeef");
        assert_eq!(
            fwd.get("x-sandbox-namespace").unwrap(),
            "agent-sandbox-runtime"
        );
        assert_eq!(fwd.get("x-sandbox-pod-ip").unwrap(), "10.0.0.5");
        assert_eq!(fwd.get("x-session-id").unwrap(), "chat-s");
    }

    #[test]
    fn forward_headers_omit_authorization_when_runtime_key_unconfigured() {
        let h = headers_with(Some("u"), None, None, Some("owui-bearer"));
        let fwd = build_forward_headers(&h, &resolved(), "ns", "u", "");
        assert!(
            fwd.get("authorization").is_none(),
            "no Bearer when runtime key unset"
        );
        assert_eq!(fwd.get("x-sandbox-id").unwrap(), "owui-c-deadbeef");
    }

    #[test]
    fn forward_headers_preserve_caller_headers() {
        let mut h = headers_with(Some("u"), None, None, None);
        h.insert("x-custom", HeaderValue::from_static("keep-me"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        let fwd = build_forward_headers(&h, &resolved(), "ns", "u", "k");
        assert_eq!(fwd.get("x-custom").unwrap(), "keep-me");
        assert_eq!(fwd.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn hop_set_is_complete() {
        // The HOP set, verbatim.
        for h in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailers",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            "authorization",
        ] {
            assert!(hop_set().contains(&h), "missing {h}");
        }
    }

    // --- upstream_url / rewrite_location -----------------------------------

    #[test]
    fn upstream_url_targets_pod_ip_and_8888() {
        let state = AppState::for_test(shared::BrokerConfig::default());
        let url = upstream_url(&state, &resolved(), "/execute?x=1");
        assert_eq!(url, "http://10.0.0.5:8888/execute?x=1");
    }

    #[test]
    fn upstream_url_honours_override_base() {
        let state = AppState::for_test(shared::BrokerConfig::default())
            .with_runtime_upstream_override("http://127.0.0.1:9999");
        let url = upstream_url(&state, &resolved(), "/files/list");
        assert_eq!(url, "http://127.0.0.1:9999/files/list");
    }

    #[test]
    fn rewrite_location_strips_host() {
        assert_eq!(
            rewrite_location("http://10.0.0.5:8888/files/list/."),
            "/files/list/."
        );
        assert_eq!(rewrite_location("/foo?bar=1"), "/foo?bar=1");
        assert_eq!(rewrite_location("relative/path"), "relative/path");
    }
}
