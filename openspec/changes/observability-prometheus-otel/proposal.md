# Observability: Prometheus metrics + OpenTelemetry traces

## Why

open-websandbox ships two FastAPI services (broker + runtime) backing Open WebUI's
"Open Terminal", but offers no first-class observability story. The runtime already
exposes a basic `runtime_http_requests_total` counter; the broker a `broker_http_requests_total`
counter. Neither follows a shared naming convention, neither emits latency, and there is
no tracing, no scrape configuration, and no dashboard. Issue #5 asks for a complete
Prometheus + OpenTelemetry layer so operators can see request rate/latency/errors, active
sandboxes, and the broker->runtime hop in one place.

## Proposal

Add Prometheus metrics + OpenTelemetry tracing with a consistent `open_websandbox_` prefix,
bring-your-own OTel collector by default, and an optional bundled `otel-collector` subchart.

- **Broker `/metrics`** — extend the existing endpoint with HTTP request rate/latency/errors
  (Histogram + Counter, `method`+`path`+`status` labels), an active-sandboxes gauge,
  sandbox create/delete counters, and a runtime-hop error counter. Prefix all names
  `open_websandbox_`.
- **Runtime `/metrics`** — keep the existing endpoint; rename the counter to the
  `open_websandbox_` prefix (consistency with owner decision D1); add OTel.
- **OTel traces** (broker + runtime) — optional/soft auto-instrumentation (FastAPI + httpx)
  exporting via OTLP (`OTEL_EXPORTER_OTLP_ENDPOINT`). Bring-your-own: no collector deployed
  by default. Soft-import so the services stay up when OTel libs/endpoint are absent.
- **Chart** — a `ServiceMonitor` for broker + runtime, `interval: 60s`, gated behind
  `monitoring.prometheus.enabled` (off by default so it doesn't fail clusters without the CRD).
  An optional bundled `opentelemetry-collector` subchart (`monitoring.otelCollector.enabled`,
  off by default) wired as the OTLP endpoint when enabled.
- **Grafana dashboard** — a standalone JSON in `docs/` covering the key metrics.
- **values.yaml** — a `monitoring:` section (prometheus.enabled, otelCollector.enabled,
  otlp endpoint, scrape interval 60s), documented.

## Decisions

- **D1 Naming** — all metrics use the `open_websandbox_` prefix (Prometheus best practices).
- **D2 OTel collector** — bring-your-own by default (operators point at their collector via
  OTLP); ship an optional bundled `otel-collector` subchart, OFF by default.
- **D3 Scrape interval** — 60s (per-minute is sufficient).
- **D4 Soft OTel import** — OTel instrumentation is imported inside try/except and only
  activates when configured; the broker/runtime boot and serve regardless. Keeps unit tests
  green without installing OTel libs and matches the bring-your-own philosophy.
- **D5 Conflict avoidance with PR #4** — broker auth functions (`_auth`, `_runtime_auth_headers`,
  `_validate_config`) are NOT touched; broker changes are confined to the `/metrics` endpoint,
  the OTel setup/middleware, and the chart monitoring objects.

## Non-goals

- Changing the vendored upstream controller / CRDs / namespaces (byte-for-byte preserved).
- Removing or replacing the runtime's existing Prometheus exposition (extended, not removed).
- Deploying a collector by default (operators opt in).
