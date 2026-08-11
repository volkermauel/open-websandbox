//! Soft OpenTelemetry tracing initialisation shared by the `broker` and
//! `runtime` binaries (D9, issue #74 Q3 → "support both"; issue #83 hardening).
//!
//! Behaviour (matches the `observability-prometheus-otel` spec's "no collector
//! configured" scenario):
//!
//! - **Always** installs a `tracing_subscriber` with a `fmt` layer + env filter.
//! - When `OTEL_EXPORTER_OTLP_ENDPOINT` is set (and the `telemetry-otlp` feature
//!   is enabled), additionally builds an OTLP span exporter + a
//!   `SdkTracerProvider` and bridges it into `tracing` via
//!   [`tracing_opentelemetry::layer`]. The provider is returned so the caller
//!   can `shutdown()` it on graceful exit (best-effort — the batch processor
//!   flushes periodically regardless).
//! - The OTLP transport is selected by `OTEL_EXPORTER_OTLP_PROTOCOL`
//!   (`grpc` [default] or `http`); see [`OTEL_PROTOCOL_ENV`].
//! - When the endpoint env var is unset/empty, OR building the exporter fails,
//!   OR the `telemetry-otlp` feature is compiled out, tracing is a no-op: the
//!   subscriber falls back to `fmt`-only. **Boot and serve never depend on a
//!   collector being reachable** — exporter construction is lazy (no
//!   connection); an unreachable collector just drops/retries spans.
//!
//! ## Feature gate (issue #83)
//! The OTLP exporter crate (`opentelemetry-otlp`, which pulls tonic/prost/h2 for
//! gRPC and reqwest for HTTP) is behind the default-on `telemetry-otlp` feature.
//! Slim/no-OTel builds compile with `--no-default-features` to drop the whole
//! gRPC/HTTP client stack. The `metrics` facade + Prometheus `/metrics` and the
//! `opentelemetry` tracing SDK are always-on and unaffected — only the OTLP
//! *exporter transport* is gated.

#![forbid(unsafe_code)]

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
#[cfg(feature = "telemetry-otlp")]
use opentelemetry_sdk::Resource;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Env var that, when set, opts into the OTLP exporter (collector endpoint).
pub const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Env var selecting the OTLP transport when the exporter is opted into.
/// `grpc` (default) uses the tonic transport; `http` uses the HTTP/protobuf
/// exporter (`http/protobuf` is accepted as an alias). Any other value makes
/// [`init`] fall back to fmt-only tracing (the build error is logged, never
/// fatal).
pub const OTEL_PROTOCOL_ENV: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";

/// Default OTLP transport when [`OTEL_PROTOCOL_ENV`] is unset.
pub const DEFAULT_OTLP_PROTOCOL: &str = "grpc";

/// Initialise the global `tracing` subscriber.
///
/// Installs a `fmt` layer always; adds the OTel/OTLP bridge only when
/// [`OTEL_ENDPOINT_ENV`] is set (and the `telemetry-otlp` feature is enabled).
/// Returns the provider (so the caller can flush on shutdown) when OTel is
/// active, else `None`.
///
/// # Panics
/// Panics if a global tracing subscriber is already installed (call once, from
/// `main`, before any `tracing!` macro fires).
#[must_use]
pub fn init(service_name: &str, default_filter: &str) -> Option<SdkTracerProvider> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let fmt_layer = tracing_subscriber::fmt::layer();

    // Opt-in: only when an OTLP endpoint is configured AND the exporter is
    // compiled in. With `telemetry-otlp` disabled this whole block is absent and
    // tracing is unconditionally fmt-only (no tonic/prost/h2 in the build).
    #[cfg(feature = "telemetry-otlp")]
    let provider = {
        let endpoint = std::env::var(OTEL_ENDPOINT_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty());
        endpoint.and_then(|ep| match build_provider(service_name, &ep) {
            Ok(p) => Some(p),
            Err(e) => {
                // Not fatal: fall back to fmt-only tracing so boot/serve continue.
                // (The tracing subscriber is not installed yet, so log to stderr.)
                eprintln!("OTel: OTLP provider build failed ({e}); tracing is fmt-only");
                None
            }
        })
    };
    #[cfg(not(feature = "telemetry-otlp"))]
    let provider: Option<SdkTracerProvider> = None;

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);
    match provider.as_ref() {
        Some(provider) => {
            let tracer = provider.tracer(service_name.to_string());
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            registry.with(otel_layer).init();
        }
        None => registry.init(),
    }
    provider
}

/// Build the OTLP `SdkTracerProvider` for `service_name` pointing at `endpoint`.
///
/// The transport is chosen via [`OTEL_PROTOCOL_ENV`] (default
/// [`DEFAULT_OTLP_PROTOCOL`]); `grpc` → tonic, `http` → HTTP/protobuf.
#[cfg(feature = "telemetry-otlp")]
fn build_provider(service_name: &str, endpoint: &str) -> Result<SdkTracerProvider, String> {
    let protocol = std::env::var(OTEL_PROTOCOL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_OTLP_PROTOCOL.to_string());

    let exporter = match protocol.as_str() {
        "http" | "http/protobuf" => build_http_exporter(endpoint)?,
        "grpc" => build_grpc_exporter(endpoint)?,
        other => {
            return Err(format!(
                "unsupported {OTEL_PROTOCOL_ENV}={other:?} (expected `grpc` or `http`)"
            ));
        }
    };

    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// OTLP/gRPC span exporter (tonic transport, pinned via the workspace
/// `opentelemetry-otlp { features = ["grpc-tonic"] }`).
#[cfg(feature = "telemetry-otlp")]
fn build_grpc_exporter(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, String> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|e| format!("OTLP/gRPC span exporter build: {e:?}"))
}

/// OTLP/HTTP span exporter (HTTP/protobuf over reqwest, issue #83).
#[cfg(feature = "telemetry-otlp")]
fn build_http_exporter(endpoint: &str) -> Result<opentelemetry_otlp::SpanExporter, String> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|e| format!("OTLP/HTTP span exporter build: {e:?}"))
}

/// Best-effort flush of an optional provider on graceful shutdown.
///
/// Logs (never panics) on failure; safe to call with `None`.
pub fn shutdown(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider {
        if let Err(e) = provider.shutdown() {
            eprintln!("OTel: tracer provider shutdown flushed with errors: {e:?}");
        }
    }
}
