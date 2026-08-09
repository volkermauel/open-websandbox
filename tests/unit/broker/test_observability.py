# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Observability tests for the broker /metrics surface + OTel soft-import.

Covers the issue #5 deliverables:
- the open_websandbox_ prefix is used for every custom broker metric (owner decision D1);
- the route label is the bounded templated path (cardinality-safe), never the raw URL;
- broker->runtime hop errors are counted on transport failure;
- OpenTelemetry is a complete no-op when OTEL_EXPORTER_OTLP_ENDPOINT is unset.
"""
from __future__ import annotations

from unittest.mock import AsyncMock

import main  # type: ignore[import-not-found]
from fastapi.testclient import TestClient
from prometheus_client import REGISTRY

_AUTH = {"Authorization": "Bearer test-secret"}


def test_broker_metrics_registered_with_prefix():
    """Every custom broker metric is registered under the open_websandbox_ prefix (D1)."""
    collectors = set(REGISTRY._names_to_collectors.keys())
    expected = {
        "open_websandbox_broker_http_requests_total",
        "open_websandbox_broker_http_request_duration_seconds",
        "open_websandbox_broker_active_sandboxes",
        "open_websandbox_broker_sandboxes_created_total",
        "open_websandbox_broker_sandboxes_deleted_total",
        "open_websandbox_broker_runtime_hop_errors_total",
    }
    assert expected <= collectors, f"missing prefixed metrics: {expected - collectors}"
    # The legacy unprefixed counter must be gone.
    assert "broker_http_requests_total" not in collectors
    # Every open_websandbox collector carries the prefix by construction.
    assert all(n.startswith("open_websandbox_") for n in collectors if "open_websandbox" in n)


def test_route_label_is_bounded_templated_path(client):
    """The path label is the matched Route template (/healthz, /api/config, /{path:path}),
    never the raw URL — the catch-all proxy collapses to its template (cardinality-safe)."""
    client.get("/healthz")
    client.get("/api/config", headers=_AUTH)
    client.get("/files/list", headers=_AUTH)  # catch-all proxy -> 400, but route is matched
    text = client.get("/metrics").text
    assert 'path="/healthz"' in text
    assert 'path="/api/config"' in text
    assert 'path="/{path:path}"' in text  # catch-all collapses; no per-request cardinality


def test_runtime_hop_errors_counted_on_transport_failure(httpx_client, monkeypatch):
    """A failed broker->runtime send increments open_websandbox_broker_runtime_hop_errors_total."""
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    httpx_client.send.side_effect = main.httpx.ConnectError("boom")
    before = REGISTRY.get_sample_value("open_websandbox_broker_runtime_hop_errors_total") or 0.0
    # The proxy re-raises the transport error; Starlette turns it into a 500 here.
    with TestClient(main.app, raise_server_exceptions=False) as c:
        r = c.post("/execute", headers={**_AUTH, "X-User-Id": "u1"}, json={"command": "echo"})
    assert r.status_code == 500
    after = REGISTRY.get_sample_value("open_websandbox_broker_runtime_hop_errors_total") or 0.0
    assert after > before


def test_otel_is_noop_without_endpoint(monkeypatch):
    """With no OTEL_EXPORTER_OTLP_ENDPOINT the tracer stays the soft no-op (boot-safe)."""
    monkeypatch.delenv("OTEL_EXPORTER_OTLP_ENDPOINT", raising=False)
    tracer_before = main._tracer
    main._setup_telemetry(main.app, "open-websandbox-broker", client=main._client)
    assert main._tracer is tracer_before
    # A hop span is a no-op CM that absorbs attribute/event calls without error.
    with main._tracer.start_as_current_span("broker.runtime_hop") as span:
        span.set_attribute("sandbox.id", "x")
        span.set_attributes({"k": "v"})  # no-op span absorbs every call
        span.add_event("tick")
        span.record_exception(RuntimeError("nope"))
    assert isinstance(main._tracer, main._NoOpTracer)
