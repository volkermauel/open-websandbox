//! Frozen-snapshot test for the broker's generated `OpenAPI` document (D10 / issue #75 Q2).
//!
//! The OWUI-facing shape must not drift silently: this serializes the merged broker +
//! runtime document to a stable, key-sorted JSON (round-tripped through `serde_json::Value`
//! so object keys are BTreeMap-sorted regardless of utoipa insertion order) and asserts
//! byte-equality against [`FIXTURE`]. `info.version` is pinned to the broker crate version
//! at compile time (issue #75 Q4); a crate-version bump must regenerate the fixture
//! (`cargo test -p broker openapi_snapshot -- --nocapture` after updating it) — an
//! intentional release-flow reminder, not CI noise.

#![forbid(unsafe_code)]

use broker::openapi::openapi_document;

const FIXTURE: &str = include_str!("openapi.snapshot.json");

/// The merged broker + runtime OWUI-facing document must match the committed snapshot,
/// byte-for-byte (canonical, key-sorted pretty JSON).
#[test]
fn openapi_matches_frozen_snapshot() {
    let doc = openapi_document();
    // Round-trip through Value so every object is key-sorted (BTreeMap); then pretty-print
    // with the same canonical formatter the fixture was generated with.
    let value = serde_json::to_value(&doc).expect("OpenApi serializes to JSON");
    let actual = serde_json::to_string_pretty(&value).expect("pretty JSON");
    assert_eq!(
        actual,
        FIXTURE.trim_end(),
        "broker OpenAPI snapshot drifted. If this is intentional, regenerate \
         `rust/broker/tests/openapi.snapshot.json` from `openapi_document()` \
         (`serde_json::to_string_pretty(&serde_json::to_value(&doc).unwrap())`)."
    );
}

/// issue #75 Q4: `info.version` must equal `broker::version()` (== `CARGO_PKG_VERSION`).
#[test]
fn info_version_tracks_crate_version() {
    let doc = openapi_document();
    assert_eq!(doc.info.version, broker::version());
}

/// Sanity: the gated surface shows the Bearer lock and the open probes do not, and the
/// runtime surface (issue #75 Q1 = "all") made it into the merged document.
#[test]
fn merged_document_has_expected_surfaces() {
    let doc = openapi_document();
    let paths = &doc.paths.paths;
    // Broker-owned.
    assert!(
        paths.contains_key("/api/sandboxes/{name}"),
        "sandbox CRUD present"
    );
    assert!(
        paths.contains_key("/api/config"),
        "broker gated surface present"
    );
    // Runtime surface (proxied) merged in.
    assert!(paths.contains_key("/execute"), "runtime /execute merged in");
    assert!(
        paths.contains_key("/files/list"),
        "runtime /files surface merged in"
    );
    assert!(
        paths.contains_key("/api/terminals"),
        "runtime terminals merged in"
    );
    // Catch-all proxy + WS upgrade intentionally omitted (issue #75 Q5).
    assert!(!paths.contains_key("/{*path}"), "catch-all proxy omitted");
    // Bearer scheme registered.
    let schemes = doc
        .components
        .as_ref()
        .expect("components present")
        .security_schemes
        .get(shared::BEARER_SCHEME);
    assert!(schemes.is_some(), "brokerBearer security scheme registered");
}
