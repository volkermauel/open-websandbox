//! Soft OpenTelemetry tracing initialisation shared by the `broker` and
//! `runtime` binaries (D9, issue #74 Q3 → "support both").
//!
//! Behaviour (matches the `observability-prometheus-otel` spec's "no collector
//! configured" scenario):
//!
//! - **Always** installs a `tracing_subscriber` with a `fmt` layer + env filter.
//! - When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, additionally builds an OTLP/gRPC
//!   span exporter + a `SdkTracerProvider` and bridges it into `tracing` via
//!   [`tracing_opentelemetry::layer`]. The provider is returned so the caller
//!   can `shutdown()` it on graceful exit (best-effort — the batch processor
//!   flushes periodically regardless).
//! - When the env var is unset/empty, OR building the exporter fails, tracing
//!   is a no-op: the subscriber falls back to `fmt`-only. **Boot and serve
//!   never depend on a collector being reachable** — exporter construction is
//!   lazy (no connection); an unreachable collector just drops/retries spans.
//!
//! The OTLP exporter uses the gRPC/tonic transport (pinned once via the
//! workspace `opentelemetry-otlp { features = ["trace", "grpc-tonic"] }`).

#![forbid(unsafe_code)]

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Env var that, when set, opts into the OTLP exporter (collector endpoint).
pub const OTEL_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Initialise the global `tracing` subscriber.
///
/// Installs a `fmt` layer always; adds the OTel/OTLP bridge only when
/// [`OTEL_ENDPOINT_ENV`] is set. Returns the provider (so the caller can flush
/// on shutdown) when OTel is active, else `None`.
///
/// # Panics
/// Panics if a global tracing subscriber is already installed (call once, from
/// `main`, before any `tracing!` macro fires).
#[must_use]
pub fn init(service_name: &str, default_filter: &str) -> Option<SdkTracerProvider> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let fmt_layer = tracing_subscriber::fmt::layer();

    // Opt-in: only when an OTLP endpoint is configured.
    let endpoint = std::env::var(OTEL_ENDPOINT_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());

    let provider = endpoint.and_then(|ep| match build_provider(service_name, &ep) {
        Ok(p) => Some(p),
        Err(e) => {
            // Not fatal: fall back to fmt-only tracing so boot/serve continue.
            // (The tracing subscriber is not installed yet, so log to stderr.)
            eprintln!("OTel: OTLP provider build failed ({e}); tracing is fmt-only");
            None
        }
    });

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

/// Build the OTLP/gRPC `SdkTracerProvider` for `service_name` pointing at `endpoint`.
fn build_provider(service_name: &str, endpoint: &str) -> Result<SdkTracerProvider, String> {
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_string())
        .build()
        .map_err(|e| format!("OTLP span exporter build: {e:?}"))?;
    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
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
