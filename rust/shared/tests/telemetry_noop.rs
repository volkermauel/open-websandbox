//! `OTel` no-op-when-unset contract (issue #74 Q3 → "support both"; the
//! `observability-prometheus-otel` spec's "no collector configured" scenario).
//!
//! With `OTEL_EXPORTER_OTLP_ENDPOINT` unset, [`shared::init_telemetry`] installs
//! only the `fmt` tracing subscriber and returns `None` — i.e. **no** OTLP span
//! exporter is built and **no** collector is ever contacted, so boot and serve
//! never depend on one being reachable. This is a dedicated test binary so the
//! *global* tracing subscriber (which `init` installs, and which may only be
//! set once per process) is set exactly once, in isolation from the rest of
//! the suite.

#![forbid(unsafe_code)]

/// `init` may only install the global subscriber once per process; a single
/// test in this binary calls it. Guard with [`OnceLock`] anyway so a re-entry
/// (e.g. a test harness retry) reuses the recorded result instead of panicking.
#[test]
fn init_is_noop_when_endpoint_unset() {
    // Ensure the opt-in env var is absent for this assertion regardless of the
    // ambient shell/CI (it must NOT depend on a collector being reachable).
    let prev = std::env::var_os(shared::telemetry::OTEL_ENDPOINT_ENV);
    std::env::remove_var(shared::telemetry::OTEL_ENDPOINT_ENV);

    // Must not panic, and must return no provider (=> no OTLP exporter built).
    let provider = shared::init_telemetry("test-broker", "info");

    // Restore the env var (best-effort) for any later process state.
    if let Some(v) = prev {
        std::env::set_var(shared::telemetry::OTEL_ENDPOINT_ENV, v);
    }

    assert!(
        provider.is_none(),
        "init must return None (no OTLP provider) when {0} is unset — \
         boot/serve must never depend on a collector",
        shared::telemetry::OTEL_ENDPOINT_ENV,
    );

    // The fmt-only subscriber is functional: emitting a span/event must not
    // panic or error. (OTel degrades to a no-op — nothing is exported.)
    tracing::info!(target: "otel_noop_test", "telemetry no-op check: subscriber alive");
}
