# Specification: observability (Prometheus + OpenTelemetry)

## Requirement: prefixed Prometheus metrics

The broker and runtime SHALL expose Prometheus metrics on `/metrics` with all custom
metric names prefixed `open_websandbox_`.

### Scenario: broker metrics surface

- **WHEN** the broker serves `/metrics`
- **THEN** it exposes HTTP request rate/latency/errors (`open_websandbox_broker_http_requests_total`,
  `open_websandbox_broker_http_request_duration_seconds`), active sandboxes
  (`open_websandbox_broker_active_sandboxes`), sandbox create/delete counts
  (`open_websandbox_broker_sandboxes_created_total` / `..._deleted_total`), and runtime-hop
  errors (`open_websandbox_broker_runtime_hop_errors_total`).
- **AND** the `path` label is the matched *templated* route (bounded cardinality), not the raw URL.

## Requirement: optional OpenTelemetry tracing

The broker and runtime SHALL support OpenTelemetry auto-instrumentation (FastAPI + httpx)
exporting via OTLP when configured via `OTEL_EXPORTER_OTLP_ENDPOINT`, and SHALL remain
functional (boot + serve) when OTel libraries are absent or unconfigured.

### Scenario: no collector configured

- **WHEN** OTel libs are not importable OR `OTEL_EXPORTER_OTLP_ENDPOINT` is unset
- **THEN** the services start and serve requests normally (tracing is a no-op).

## Requirement: chart monitoring objects are opt-in

The chart SHALL render a `ServiceMonitor` for broker + runtime only when
`monitoring.prometheus.enabled` is true (default false), and SHALL render the bundled
`otel-collector` subchart only when `monitoring.otelCollector.enabled` is true (default false).

### Scenario: default install on a CRD-less cluster

- **WHEN** the chart is installed with defaults
- **THEN** no `ServiceMonitor`/`PodMonitor` is rendered (so clusters without the Prometheus CRD install cleanly).
