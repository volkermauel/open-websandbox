"""Targeted tests for the remaining coverage gaps in broker main.py.

Each test closes a specific uncovered branch (retry-loop continuations, the
non-'workspace' volume/mount skip, the no-location redirect skip, and the
reaper's "fresh — neither reap nor park" path).  Together they push broker
branch coverage from ~96% toward ~99% with tests that exercise real production
behaviour (waiting for a pod IP, a claim that becomes ready over iterations).
"""
from __future__ import annotations

import asyncio
import contextlib
import time
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import pytest
from starlette.websockets import WebSocketDisconnect

import main  # type: ignore[import-not-found]
from conftest import make_claim, make_sandbox

_AUTH = {"Authorization": "Bearer test-secret"}


def _resp(status=200, body=b"{}", headers=None, content_type="application/json"):
    r = MagicMock()
    r.status_code = status
    r.aread = AsyncMock(return_value=body)
    r.headers = headers if headers is not None else {"content-type": content_type}
    return r


# --- _create_chat_sandbox ---------------------------------------------------

def test_create_chat_sandbox_preserves_non_workspace_volumes(api, monkeypatch):
    """A non-'workspace' volume/mount in the base template is left untouched: only the
    workspace volume is rewired to the PVC and only the workspace mount gets a subPath.
    Covers the loop-iteration-but-no-match branches in _create_chat_sandbox."""
    monkeypatch.setattr(main, "_persistent_volume", lambda u: ("ws-pvc", "users/u1/"))
    template = {"spec": {"podTemplate": {"spec": {
        "volumes": [
            {"name": "workspace", "emptyDir": {}},
            {"name": "cache", "emptyDir": {}},
        ],
        "containers": [{"name": "app", "volumeMounts": [
            {"name": "workspace", "mountPath": "/workspace"},
            {"name": "cache", "mountPath": "/cache"},
        ]}],
    }}}}
    api.get_namespaced_custom_object.return_value = template
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "cs1"}}

    main._create_chat_sandbox("cs1", "u1", "sess-1")

    body = api.create_namespaced_custom_object.call_args.args[-1]
    spec = body["spec"]["podTemplate"]["spec"]
    vols = {v["name"]: v for v in spec["volumes"]}
    mounts = {m["name"]: m for m in spec["containers"][0]["volumeMounts"]}
    # workspace rewired + subPath applied
    assert vols["workspace"]["persistentVolumeClaim"] == {"claimName": "ws-pvc"}
    assert vols["workspace"].get("emptyDir") is None
    assert mounts["workspace"]["subPath"].startswith("users/u1/")
    # non-workspace left exactly as-is
    assert vols["cache"] == {"name": "cache", "emptyDir": {}}
    assert "subPath" not in mounts["cache"]


# --- _resolve_chat_sandbox / _ensure_sandbox_running_ip retry loops ---------

def test_resolve_chat_sandbox_waits_for_pod_ip(api, monkeypatch, no_sleep):
    """A ready sandbox whose pod has no IP yet: the loop sleeps, refetches, and returns
    once the IP appears. Covers the no-ip skip branch and the loop-back branch."""
    no_ip = make_sandbox(name="cs1", ready=True, pod_ip=None)
    with_ip = make_sandbox(name="cs1", ready=True, pod_ip="10.0.0.9")
    monkeypatch.setattr(main, "_get_sandbox", MagicMock(side_effect=[no_ip, with_ip]))

    name, ip = asyncio.run(main._resolve_chat_sandbox("u1", "sess-1"))
    assert ip == "10.0.0.9"
    assert name == main._chat_sandbox_name("u1", "sess-1")  # the hashed per-chat name


def test_ensure_sandbox_running_ip_waits_for_ip(api, monkeypatch, no_sleep):
    """Ready but IP-less sandbox becomes reachable on the next poll. Covers the no-ip
    loop-continue branch of _ensure_sandbox_running_ip."""
    no_ip = make_sandbox(name="sb1", ready=True, pod_ip=None)
    with_ip = make_sandbox(name="sb1", ready=True, pod_ip="10.0.0.7")
    monkeypatch.setattr(main, "_get_sandbox", MagicMock(side_effect=[no_ip, with_ip]))

    assert asyncio.run(main._ensure_sandbox_running_ip("sb1", timeout=90.0)) == "10.0.0.7"


def test_ensure_sandbox_running_ip_missing(api, monkeypatch, no_sleep):
    """A vanished sandbox resolves to None immediately (no busy-wait)."""
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    assert asyncio.run(main._ensure_sandbox_running_ip("sb1", timeout=5.0)) is None


# --- resolve_sandbox ephemeral claim retry loop -----------------------------

def test_resolve_sandbox_ephemeral_claim_retry(api, monkeypatch, no_sleep):
    """The ephemeral claim walks through every retry state before succeeding: not-ready,
    ready-but-no-sandbox, ready-but-no-pod-ip, then ready-with-ip. Covers the three
    skip/loop-back branches of the main claim loop."""
    monkeypatch.setattr(main, "_get_claim", MagicMock(side_effect=[
        make_claim("c1", ready=False),                                  # iter 0 (initial get)
        make_claim("c1", ready=True, sandbox=None),                     # ready, no sandbox id
        make_claim("c1", ready=True, sandbox="sbx-1", pod_ip=None),     # ready, no pod ip
        make_claim("c1", ready=True, sandbox="sbx-1", pod_ip="10.0.0.5"),  # success
    ]))
    monkeypatch.setattr(main, "_create_claim", MagicMock(return_value=None))

    sid, ip = asyncio.run(main.resolve_sandbox("u1", "sess-1", main.EPHEMERAL))
    assert sid == "sbx-1"
    assert ip == "10.0.0.5"


# --- proxy redirect without a Location header -------------------------------

def test_proxy_redirect_without_location(client, httpx_client, monkeypatch):
    """A 3xx upstream response carrying no Location header is passed through untouched
    (the rewrite is skipped). Covers the `if loc:` False branch in proxy()."""
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    httpx_client.send.return_value = _resp(307, b"", headers={"content-type": "text/plain"})
    r = client.get("/files/list/", headers={**_AUTH, "X-User-Id": "u1"}, follow_redirects=False)
    assert r.status_code == 307
    assert "location" not in r.headers


# --- reaper: fresh claims/sandboxes are neither reaped nor parked -----------

@pytest.fixture
def reaper_one_tick(monkeypatch):
    """Cancel the reaper after its first full iteration (sleep raises CancelledError)."""
    async def _break(*_a, **_k):
        raise asyncio.CancelledError()
    monkeypatch.setattr(main.asyncio, "sleep", _break)


async def _run_once():
    task = asyncio.create_task(main._reaper_loop())
    try:
        await task
    except BaseException:  # CancelledError after one tick
        pass


async def test_reaper_persistent_fresh_claim_kept(api, monkeypatch, reaper_one_tick):
    """A persistent claim idle < PARK_TTL is neither reaped nor parked: covers the
    persistent `elif idle > PARK_TTL` False branch (continue to next claim)."""
    fresh = time.time_ns() // 1_000_000_000  # idle ~0
    claim = make_claim(name="c1", profile="persistent", last_used=fresh, sandbox="sbx-1")
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    parked, deleted = [], []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    monkeypatch.setattr(main, "_delete_claim", lambda n: deleted.append(n))
    await _run_once()
    assert parked == [] and deleted == []


async def test_reaper_chat_sandbox_fresh_kept(api, monkeypatch, reaper_one_tick):
    """A chat sandbox idle < PARK_TTL is neither reaped nor parked: covers the chat
    `elif sidle > PARK_TTL` False branch (continue to next sandbox)."""
    fresh = time.time_ns() // 1_000_000_000
    sbx = make_sandbox(name="owui-c-1", last_used=fresh)
    api.list_namespaced_custom_object.side_effect = [{"items": []}, {"items": [sbx]}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    parked, deleted = [], []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert parked == [] and deleted == []


# --- terminal WS relay internals ------------------------------------------

def test_terminal_upstream_send_failure_closes_relay(client, monkeypatch, httpx_client):
    """If forwarding a client message to the upstream WS raises, the c2u relay swallows
    the error and ends; FIRST_COMPLETED then tears the session down. Covers the c2u
    except arm."""
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))

    class _BrokenSend:
        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return False

        def __aiter__(self):
            return self

        async def __anext__(self):
            await asyncio.sleep(3600)  # u2c blocks; cancelled during teardown
            raise StopAsyncIteration

        async def send(self, _data):
            raise RuntimeError("upstream gone")

        async def close(self):
            pass

    fake = _BrokenSend()
    monkeypatch.setattr(main, "websockets", SimpleNamespace(connect=lambda *a, **k: fake))
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        ws.send_bytes(b"data")  # c2u forwards -> send() raises -> except -> c2u ends
        with contextlib.suppress(WebSocketDisconnect):
            ws.receive_bytes()  # server tears down -> client disconnects


def test_terminal_upstream_ends_cleanly(client, monkeypatch, httpx_client):
    """When the upstream WS ends immediately (StopAsyncIteration, no messages), the u2c
    relay loop exits normally; FIRST_COMPLETED tears the session down. Covers the u2c
    async-for loop-exit branch."""
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))

    class _ClosingUp:
        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return False

        def __aiter__(self):
            return self

        async def __anext__(self):
            raise StopAsyncIteration  # no messages -> u2c loop exits

        async def send(self, _data):
            pass

        async def close(self):
            pass

    fake = _ClosingUp()
    monkeypatch.setattr(main, "websockets", SimpleNamespace(connect=lambda *a, **k: fake))
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        with contextlib.suppress(WebSocketDisconnect):
            ws.receive_bytes()  # u2c ends -> teardown -> client disconnects


# --- fail-closed auth, readiness probe, graceful shutdown -------------------

def test_validate_config_rejects_weak_secret(monkeypatch):
    for bad in ["", "dev-shared-secret-change-me", "change-me", "placeholder"]:
        monkeypatch.setattr(main, "SHARED_SECRET", bad)
        with pytest.raises(RuntimeError):
            main._validate_config()


def test_validate_config_accepts_strong_secret(monkeypatch):
    monkeypatch.setattr(main, "SHARED_SECRET", "a-very-strong-and-random-secret-123456")
    main._validate_config()  # no raise


def test_readyz_ok_when_apiserver_reachable(client, api):
    api.list_namespaced_custom_object.return_value = {"items": []}
    assert client.get("/readyz").status_code == 200


def test_readyz_503_when_apiserver_down(client, api):
    api.list_namespaced_custom_object.side_effect = RuntimeError("timeout")
    assert client.get("/readyz").status_code == 503


def test_stop_reaper_cancels_task_and_closes_client(monkeypatch):
    fake_task = MagicMock()
    fake_task.done.return_value = False
    monkeypatch.setattr(main, "_reaper_task", fake_task)
    fake_client = AsyncMock()
    monkeypatch.setattr(main, "_client", fake_client)
    asyncio.run(main._stop_reaper())
    fake_task.cancel.assert_called_once()
    fake_client.aclose.assert_awaited_once()


def test_stop_reaper_noop_when_no_task(monkeypatch):
    monkeypatch.setattr(main, "_reaper_task", None)
    fake_client = AsyncMock()
    monkeypatch.setattr(main, "_client", fake_client)
    asyncio.run(main._stop_reaper())  # no task to cancel; client still closed
    fake_client.aclose.assert_awaited_once()


def test_metrics_endpoint(client):
    r = client.get("/metrics")
    assert r.status_code == 200
    assert "broker_http_requests_total" in r.text  # the request counter is exposed


def test_stop_reaper_swallows_aclose_error(monkeypatch):
    monkeypatch.setattr(main, "_reaper_task", None)
    bad_client = AsyncMock()
    bad_client.aclose.side_effect = RuntimeError("close failed")
    monkeypatch.setattr(main, "_client", bad_client)
    asyncio.run(main._stop_reaper())  # aclose raises but shutdown still completes cleanly
    bad_client.aclose.assert_awaited_once()


def test_metrics_middleware_counts_500(monkeypatch):
    """An unhandled handler exception must still increment the 500 label before re-raising."""
    from prometheus_client import REGISTRY
    from fastapi.testclient import TestClient

    def _boom(*_a, **_k):
        raise RuntimeError("boom")

    # The auth-protected catch-all calls resolve_sandbox; make it raise (not HTTPException)
    # so the request-counting middleware's except-arm runs.
    monkeypatch.setattr(main, "resolve_sandbox", _boom)

    auth = {"Authorization": f"Bearer {main.SHARED_SECRET}", "X-User-Id": "u"}
    # raise_server_exceptions=False so the middleware incs the 500 label and Starlette
    # returns a real 500 rather than re-raising in the test process.
    with TestClient(main.app, raise_server_exceptions=False) as c:
        r = c.get("/files/list", headers=auth)
        assert r.status_code == 500

    val = REGISTRY.get_sample_value(
        "broker_http_requests_total", {"method": "GET", "status": "500"}
    )
    assert val and val >= 1.0
