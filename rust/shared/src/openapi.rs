//! Cross-crate OpenAPI helpers shared by the broker and runtime crates (D10).
//!
//! Both the broker and runtime surface a Bearer (shared-secret / per-session-key)
//! auth scheme and an identical `{"detail": "..."}` error body, so those live here
//! and are reused by both `#[derive(OpenApi)]` documents. The broker OWUI-facing
//! document is built from the broker's own paths and the runtime's `RuntimeApiDoc`
//! merged in (issue #75 Q1 = "all").

#![forbid(unsafe_code)]

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::Modify;

/// The security-scheme name operations reference via `security(("brokerBearer" = []))`.
///
/// Gated broker paths use the OWUI→broker shared secret; gated runtime paths use the
/// per-session key — both ride the same `HTTP Bearer` scheme name so the OpenAPI 🔒
/// resolves consistently across the merged document.
pub const BEARER_SCHEME: &str = "brokerBearer";

/// Registers the [`BEARER_SCHEME`] HTTP Bearer security scheme on an [`OpenApi`] document.
///
/// Used as a `modifiers(...)` entry by both the broker and runtime `#[derive(OpenApi)]`
/// structs so gated operations show the 🔒 lock. Adding the scheme in both crates keeps
/// each document valid standalone; the broker's `OpenApi::merge` dedupes by scheme name.
#[derive(Default)]
pub struct BearerAddon;

impl Modify for BearerAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            BEARER_SCHEME,
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
    }
}

/// Error body both control planes return: `{"detail": "..."}` (the
/// `{ "detail": <string> }` shape — byte-for-byte parity, D11). Used as the
/// error response `body` across the broker and runtime OpenAPI surfaces.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ErrorResponse {
    /// Human-readable error detail.
    pub detail: String,
}
