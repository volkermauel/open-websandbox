//! OTLP/HTTP transport opt-in contract (issue #83).
//!
//! With the `telemetry-otlp` feature enabled and `OTEL_EXPORTER_OTLP_PROTOCOL=http`,
//! [`shared::init_telemetry`] must build the OTLP **HTTP** exporter
//! (HTTP/protobuf over reqwest-rustls) and return `Some(provider)` — i.e. the
//! HTTP path is wired and the exporter constructs without a live collector
//! (construction is lazy: no connection is attempted at build time). This mirrors
//! the existing no-op contract (`telemetry_noop.rs`): a separate test binary so
//! the *global* tracing subscriber (which `init` installs exactly once) is set in
//! an isolated process.
//!
//! Gated on `telemetry-otlp`: when the feature is compiled out the OTLP exporter
//! crate is absent and `init` is unconditionally fmt-only, so this binary carries
//! no tests in that configuration.

#![cfg(feature = "telemetry-otlp")]
#![forbid(unsafe_code)]

use shared::telemetry::{OTEL_ENDPOINT_ENV, OTEL_PROTOCOL_ENV};

/// Setting `OTEL_EXPORTER_OTLP_PROTOCOL=http` makes `init` build the HTTP/protobuf
/// exporter instead of the default tonic/gRPC one. No collector is reachable; the
/// exporter build is lazy, so this proves the HTTP path constructs end-to-end.
#[test]
fn init_builds_http_exporter_when_protocol_http() {
    // Explicitly-fake collector endpoint; exporter build never connects.
    let prev_ep = std::env::var_os(OTEL_ENDPOINT_ENV);
    let prev_proto = std::env::var_os(OTEL_PROTOCOL_ENV);
    std::env::set_var(OTEL_ENDPOINT_ENV, "http://127.0.0.1:4318");
    std::env::set_var(OTEL_PROTOCOL_ENV, "http");

    let provider = shared::init_telemetry("test-http", "info");

    // Restore env (best-effort) for any later process state.
    restore_env(OTEL_ENDPOINT_ENV, prev_ep);
    restore_env(OTEL_PROTOCOL_ENV, prev_proto);

    let provider = provider.expect(
        "init must return Some(provider) when OTEL_EXPORTER_OTLP_ENDPOINT is set and \
         OTEL_EXPORTER_OTLP_PROTOCOL=http — the HTTP exporter must build without a \
         reachable collector (build is lazy)",
    );

    // Cleanly tear down the provider (logs, never panics, on failure).
    shared::shutdown_telemetry(Some(provider));
}

fn restore_env(key: &str, prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
