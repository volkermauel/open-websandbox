# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Shared fixtures for broker unit tests.

The broker (``open-websandbox-platform/broker/main.py``) has three import-time side
effects that must be neutralised *before* ``import main``:

1. ``config.load_incluster_config()`` / ``load_kube_config()`` — fail outside a cluster.
2. ``api = client.CustomObjectsApi()`` + ``core = client.CoreV1Api()`` — module globals
   that every k8s helper calls. Replaced with two shared, controllable ``MagicMock``
   singletons so per-test ``return_value``/``side_effect`` configuration is enough.
3. ``@app.on_event("startup")`` spawns an infinite ``_reaper_loop``. We clear
   ``app.router.on_startup`` so a context-managed ``TestClient`` does not start it; the
   reaper is exercised directly via :func:`run_reaper_once`.

``main._client`` (an ``httpx.AsyncClient``) is left as the real object at import but the
function-scoped ``httpx_client`` fixture swaps in an ``AsyncMock`` for any test that drives
the proxy / migrate / terminal paths.
"""
from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest

# (1) deterministic env BEFORE import so module globals are stable -------------------
os.environ.setdefault("BROKER_SHARED_SECRET", "test-secret")
os.environ.setdefault("BROKER_WARMPOOL", "test-warmpool")
os.environ.setdefault("BROKER_RUNTIME_NS", "test-runtime-ns")
os.environ.setdefault("BROKER_ROUTER_URL", "http://router.test:8080")
os.environ.setdefault("BROKER_IDLE_TTL_SECONDS", "120")
os.environ.setdefault("BROKER_PARK_IDLE_SECONDS", "120")
os.environ.setdefault("BROKER_REAP_SECONDS", str(7 * 24 * 3600))
os.environ.setdefault("BROKER_CLAIM_TIMEOUT_SECONDS", "60")
os.environ.setdefault("BROKER_PROXY_TIMEOUT_SECONDS", "660")
os.environ.setdefault("BROKER_DEFAULT_PROFILE", "persistent")
os.environ.setdefault("BROKER_PERSISTENT_MODE", "per-user-pvc")

# (2) neutralise kubernetes import-time side effects --------------------------------
import kubernetes.client as _kc  # noqa: E402
import kubernetes.config as _kcfg  # noqa: E402

_kcfg.load_incluster_config = lambda *a, **k: None
_kcfg.load_kube_config = lambda *a, **k: None

# Two controllable singletons — installed as main.api / main.core at import.
_api = MagicMock(name="broker_api")
_core = MagicMock(name="broker_core")
_kc.CustomObjectsApi = lambda *a, **k: _api
_kc.CoreV1Api = lambda *a, **k: _core

# Default per-session key Secret read (issue #4): proxy/terminal/resolve tests that
# don't override read_namespaced_secret still get a valid key back so _runtime_auth_headers
# resolves a Bearer instead of raising on the MagicMock default. Tests that exercise the
# 404 / error arms override side_effect explicitly.
import base64 as _b64  # noqa: E402
from types import SimpleNamespace as _SNS  # noqa: E402

_core.read_namespaced_secret.return_value = _SNS(
    data={"api-key": _b64.b64encode(b"test-per-session-key").decode()})

# (3) make `import main` work -------------------------------------------------------
_BROKER_DIR = Path(__file__).resolve().parents[3] / "open-websandbox-platform" / "broker"
if str(_BROKER_DIR) not in sys.path:
    sys.path.insert(0, str(_BROKER_DIR))

import main  # type: ignore[import-not-found]  # noqa: E402,F401

# Stop the lifespan handler from spawning the infinite reaper under TestClient.
main.app.router.on_startup[:] = []

# Kept for direct unit testing of the reaper (no lifespan involvement).
REAL_REAPER = main._reaper_loop


def make_claim(name: str = "c1", ready: bool = True, sandbox: str | None = "sbx-1",
               pod_ip: str | None = "10.0.0.1", last_used: int = 1,
               profile: str = "ephemeral") -> dict:
    """A realistic SandboxClaim dict for resolve/reaper tests."""
    return {
        "metadata": {"name": name, "namespace": main.RUNTIME_NS,
                     "labels": {**main.MANAGED_BY, main.PROFILE: profile},
                     "annotations": {main.LAST_USED: str(last_used)}},
        "status": {
            "conditions": ([{"type": "Ready", "status": "True"}] if ready else []),
            "sandbox": ({"name": sandbox, "podIPs": ([pod_ip] if pod_ip else [])}
                        if sandbox else {}),
        },
    }


def make_sandbox(name: str = "sbx-1", ready: bool = True, pod_ip: str | None = "10.0.0.1",
                 operating_mode: str = "Running", last_used: int = 1,
                 profile: str = "persistent", chat: bool = True) -> dict:
    """A realistic per-session Sandbox dict for resolve/reaper/migrate tests (issue #4).

    All broker-owned sandboxes are direct `agents.x-k8s.io/Sandbox` objects labeled
    managed-by=owui-broker + broker-profile (ephemeral|persistent); the reaper selects on
    managed-by=owui-broker. `chat=True` adds broker-chat=true (persistent per-chat)."""
    labels = {**main.MANAGED_BY, main.PROFILE: profile}
    if chat:
        labels["broker-chat"] = "true"
    return {
        "metadata": {"name": name, "namespace": main.RUNTIME_NS,
                     "labels": labels,
                     "annotations": {main.LAST_USED: str(last_used)}},
        "spec": {"operatingMode": operating_mode},
        "status": {
            "conditions": ([{"type": "Ready", "status": "True"}] if ready else []),
            "podIPs": ([pod_ip] if pod_ip else []),
        },
    }


def api_exc(status: int):
    """A kubernetes ApiException with the given HTTP status (for side_effect tests)."""
    return main.client.ApiException(status=status)


# --- fixtures ---------------------------------------------------------------------
@pytest.fixture
def api(monkeypatch):
    """Fresh controllable CustomObjectsApi mock, installed as main.api (auto-restored)."""
    mock = MagicMock(name="api")
    monkeypatch.setattr(main, "api", mock)
    return mock


@pytest.fixture
def core(monkeypatch):
    """Fresh controllable CoreV1Api mock, installed as main.core (auto-restored)."""
    mock = MagicMock(name="core")
    monkeypatch.setattr(main, "core", mock)
    return mock


@pytest.fixture
def client():
    """FastAPI TestClient. on_startup is cleared (no reaper); lifespan is otherwise inert."""
    from fastapi.testclient import TestClient
    with TestClient(main.app) as c:
        yield c


@pytest.fixture
def httpx_client():
    """Replace main._client with a controllable AsyncMock; restore on teardown."""
    original = main._client
    mock = AsyncMock(name="httpx_client")
    main._client = mock
    try:
        yield mock
    finally:
        main._client = original


@pytest.fixture
def no_sleep(monkeypatch):
    """Make asyncio.sleep a no-op so resolve loops don't really wait."""
    async def _noop(_t):
        return None
    monkeypatch.setattr(main.asyncio, "sleep", _noop)


@pytest.fixture
def fresh_migrate_locks():
    """Clear per-user migrate locks so asyncio.Lock objects aren't bound to a dead loop."""
    main._migrate_locks.clear()


class FakeUpstream:
    """Async CM + async iterator standing in for ``websockets.connect(url)``.

    Yields the given messages (bytes or str), then BLOCKS forever on the next
    ``__anext__`` (so ``_upstream_to_client`` doesn't end before the test
    disconnects). Records everything the proxy forwarded via ``.sent``. Use it
    to drive the terminal-WS success path deterministically under TestClient.
    """
    def __init__(self, messages=None):
        self._messages = list(messages or [])
        self.sent: list = []
        self.closed = False
        self._idx = 0

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        self.closed = True
        return False

    def __aiter__(self):
        return self

    async def __anext__(self):
        if self._idx < len(self._messages):
            m = self._messages[self._idx]
            self._idx += 1
            return m
        # Block forever — cancelled during FIRST_COMPLETED cleanup.
        await asyncio.sleep(3600)
        raise StopAsyncIteration

    async def send(self, data):
        self.sent.append(data)

    async def close(self):
        self.closed = True


@pytest.fixture
def patch_websockets(monkeypatch):
    """Replace ``main.websockets.connect`` with a FakeUpstream factory.

    Usage: ``up = patch_websockets([b'hello', 'world'])`` returns the FakeUpstream
    that will be used; the proxy's forwarded bytes/text land in ``up.sent``.
    """
    from types import SimpleNamespace
    created: list[FakeUpstream] = []

    def _factory(messages):
        up = FakeUpstream(messages)
        created.append(up)
        monkeypatch.setattr(main, "websockets", SimpleNamespace(connect=lambda *a, **k: up))
        return up

    return _factory
