//! OTel span-emission contract (issue #74 Q3 → "support both"; the
//! `observability-prometheus-otel` spec's "collector configured" scenario).
//!
//! Asserts the OTel layer actually **produces spans**: spans emitted via
//! `tracing` (the same path the broker/runtime instrumentation uses) are
//! bridged into OpenTelemetry and captured by an in-memory span exporter. This
//! is a dedicated test binary using [`opentelemetry_sdk`]'s
//! `InMemorySpanExporter` behind a synchronous `SimpleSpanProcessor` — **no
//! live OTLP collector pod and no network**.

#![forbid(unsafe_code)]

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{
    InMemorySpanExporterBuilder, SdkTracerProvider, SimpleSpanProcessor,
};
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;

/// Spans emitted through `tracing` are bridged into OTel and exported; the
/// representative `sandbox.resolve` (parent) + `runtime.hop` (child) shapes are
/// recorded with the correct parent/child link.
#[test]
fn otel_layer_records_representative_spans() {
    // In-memory exporter + synchronous processor: a span is exported the moment
    // it ends, so no async batch flush (and no Tokio runtime) is required.
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .with_resource(Resource::builder().with_service_name("test-broker").build())
        .build();
    let tracer = provider.tracer("test");
    // Bridge the tracer into `tracing` (exactly what shared::telemetry::init
    // does when OTEL_EXPORTER_OTLP_ENDPOINT is set, minus the OTLP transport).
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    // Emit spans under a scoped (thread-local) subscriber — not the global
    // default — so this stays isolated from any other test/binary.
    tracing::subscriber::with_default(subscriber, || {
        // Representative broker/runtime instrumentation shapes (issue #74):
        // a `sandbox.resolve` span parenting a `runtime.hop` child.
        let resolve = tracing::info_span!("sandbox.resolve", sandbox = "owui-c-deadbeef");
        resolve.in_scope(|| {
            let _hop = tracing::info_span!("runtime.hop").entered();
        });
    });

    let spans = exporter
        .get_finished_spans()
        .expect("in-memory exporter must return the exported spans");
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
    assert!(
        names.contains(&"sandbox.resolve"),
        "expected a `sandbox.resolve` span, got {names:?}"
    );
    assert!(
        names.contains(&"runtime.hop"),
        "expected a `runtime.hop` span, got {names:?}"
    );

    // The parent/child link survived the bridge: `runtime.hop`'s parent span id
    // must be `sandbox.resolve`'s span id.
    let resolve = spans
        .iter()
        .find(|s| s.name == "sandbox.resolve")
        .expect("sandbox.resolve span present");
    let hop = spans
        .iter()
        .find(|s| s.name == "runtime.hop")
        .expect("runtime.hop span present");
    assert_eq!(
        hop.parent_span_id,
        resolve.span_context.span_id(),
        "runtime.hop must be parented by sandbox.resolve"
    );

    // Cleanly tear down the provider (logs, never panics, on failure).
    let _ = provider.shutdown();
}
