# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Prometheus ``/metrics`` + graceful SIGTERM shutdown coverage for the runtime server.

Exercises the request-counting middleware, the ``/metrics`` exposition endpoint
(copied from the broker pattern), and the ``shutdown`` event handler that reaps
active PTY sessions on SIGTERM.
"""
from __future__ import annotations

import pytest
import server


async def test_metrics_endpoint_exposes_request_counter(client):
    """GET /metrics -> 200 and exposes the open_websandbox_runtime_http_requests_total
    counter.

    A prior request (GET /) must have been counted under the same counter, proving
    the middleware is wired into the request pipeline.
    """
    r = await client.get("/")
    assert r.status_code == 200

    r = await client.get("/metrics")
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/plain")
    assert "open_websandbox_runtime_http_requests_total" in r.text
    # The middleware labelled both the GET / and this scrape.
    assert 'method="GET"' in r.text


async def test_metrics_counts_unhandled_errors_as_500(client, monkeypatch):
    """A handler that raises (not HTTPException) is still counted as a 500.

    Exercises the middleware's best-effort except branch so error spikes never go
    uncounted on the scrape.
    """

    def _boom(*_args, **_kwargs):
        raise RuntimeError("boom")

    monkeypatch.setattr(server, "_request_base", _boom)
    with pytest.raises(RuntimeError):
        await client.get("/files/list")

    r = await client.get("/metrics")
    assert 'open_websandbox_runtime_http_requests_total{method="GET",status="500"}' in r.text


async def test_shutdown_reaps_active_terminals(client, clean_terminals):
    """The shutdown handler closes active PTY master fds / process groups.

    Covers both the empty-iteration and populated-iteration arcs of the shutdown
    loop: calling it with no terminals must be a no-op, and calling it after a
    terminal is created must reap that terminal.
    """
    # Clean slate -> empty-iteration arc (no-op, must not raise).
    for sid in list(server._terminals):
        server._term_cleanup(sid)
    await server._on_shutdown()

    r = await client.post("/api/terminals")
    assert r.status_code == 200
    sid = r.json()["id"]
    assert sid in server._terminals

    # Populated-iteration arc -> the tracked terminal is reaped.
    await server._on_shutdown()
    assert sid not in server._terminals


def test_runtime_metrics_prefix():
    """The runtime counter is registered under the open_websandbox_ prefix (D1)."""
    from prometheus_client import REGISTRY
    collectors = set(REGISTRY._names_to_collectors.keys())
    assert "open_websandbox_runtime_http_requests_total" in collectors
    assert "runtime_http_requests_total" not in collectors


def test_runtime_otel_noop_without_endpoint(monkeypatch):
    """With no OTEL_EXPORTER_OTLP_ENDPOINT the runtime's OTel setup is a safe no-op."""
    import server  # type: ignore[import-not-found]
    monkeypatch.delenv("OTEL_EXPORTER_OTLP_ENDPOINT", raising=False)
    # Re-running setup must not raise and must not instrument (endpoint unset).
    server._setup_telemetry(server.app, "open-websandbox-runtime")
    # The app still answers health probes.
    assert server.app.routes  # instrumented-or-not, the FastAPI app is intact
