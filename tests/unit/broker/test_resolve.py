# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Resolve tests — watch-until-ready, per-session Sandbox get-or-create, key mint /
rotate-on-resume, migrate trigger, timeout (issue #4)."""
from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import main  # type: ignore[import-not-found]
import pytest
from conftest import make_sandbox


def _ready_sb(o):
    return main._sandbox_ready(o) and bool(main._sandbox_pod_ip(o))


# --- _watch_until_ready ----------------------------------------------------------
def test_watch_initial_ready_returns_immediately(monkeypatch):
    ready = make_sandbox(ready=True, pod_ip="10.0.0.1")
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: ready
    monkeypatch.setattr(main, "api", api)
    assert main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", _ready_sb, 60) is ready


def test_watch_streams_until_ready(monkeypatch):
    not_ready = make_sandbox(ready=False)
    ready = make_sandbox(ready=True, pod_ip="10.0.0.2")
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: not_ready
    events = iter([{"raw_object": not_ready}, {"raw_object": ready}])

    class FakeWatch:
        def stream(self, *a, **k):
            yield from events

    monkeypatch.setattr(main, "api", api)
    monkeypatch.setattr("kubernetes.watch.Watch", lambda: FakeWatch())
    obj = main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", _ready_sb, 60)
    assert main._sandbox_pod_ip(obj) == "10.0.0.2"


def test_watch_on_event_resume(monkeypatch):
    not_ready = make_sandbox(ready=False)
    ready = make_sandbox(ready=True, pod_ip="10.0.0.3")
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: not_ready
    monkeypatch.setattr(main, "api", api)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    resumed = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: resumed.append((n, m)))
    events = iter([{"raw_object": ready}])

    class FakeWatch:
        def stream(self, *a, **k):
            yield from events

    monkeypatch.setattr("kubernetes.watch.Watch", lambda: FakeWatch())

    def resume(n, _o):
        if main._sandbox_operating_mode(n) == "Suspended":
            main._set_sandbox_operating_mode(n, "Running")

    main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", _ready_sb, 60, resume)
    assert ("s1", "Running") in resumed


def test_watch_missing_returns_none(monkeypatch):
    def raise404(*a, **k):
        raise main.client.ApiException(status=404)

    api = MagicMock()
    api.get_namespaced_custom_object = raise404
    monkeypatch.setattr(main, "api", api)
    assert main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", lambda o: True, 60) is None


def test_watch_deadline_expired_returns_none(monkeypatch):
    not_ready = make_sandbox(ready=False)
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: not_ready
    monkeypatch.setattr(main, "api", api)
    assert main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", lambda o: False, 0.0) is None


def test_watch_stream_error_returns_none(monkeypatch):
    not_ready = make_sandbox(ready=False)
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: not_ready

    class FakeWatch:
        def stream(self, *a, **k):
            raise RuntimeError("boom")

    monkeypatch.setattr(main, "api", api)
    monkeypatch.setattr("kubernetes.watch.Watch", lambda: FakeWatch())
    assert main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", lambda o: False, 60) is None


def test_watch_stream_exhausts_returns_none(monkeypatch):
    not_ready = make_sandbox(ready=False)
    api = MagicMock()
    api.get_namespaced_custom_object = lambda *a, **k: not_ready
    monkeypatch.setattr(main, "api", api)

    class FakeWatch:
        def stream(self, *a, **k):
            yield {"raw_object": not_ready}

    monkeypatch.setattr("kubernetes.watch.Watch", lambda: FakeWatch())
    assert main._watch_until_ready(main.SANDBOX_GROUP, "sandboxes", "s1", lambda o: False, 60) is None


def test_sandbox_ready_with_ip_predicate():
    assert main._sandbox_ready_with_ip(make_sandbox(ready=True, pod_ip="10.0.0.1"))
    assert not main._sandbox_ready_with_ip(make_sandbox(ready=True, pod_ip=None))
    assert not main._sandbox_ready_with_ip(make_sandbox(ready=False))


def test_resume_if_suspended(monkeypatch):
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    resumed = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: resumed.append((n, m)))
    main._resume_if_suspended("s1", None)
    assert resumed == [("s1", "Running")]


def test_resume_if_not_suspended(monkeypatch):
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    set_calls = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: set_calls.append((n, m)))
    main._resume_if_suspended("s1", None)
    assert set_calls == []


# --- resolve_sandbox (unified direct-Sandbox, both profiles) --------------------
async def test_resolve_ephemeral_ready(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.1", profile="ephemeral", chat=False)
    monkeypatch.setattr(main, "_ephemeral_sandbox_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_ensure_runtime_key", MagicMock())
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    name, ip = await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert name == "c1"
    assert ip == "10.0.0.1"


async def test_resolve_ephemeral_just_created_mints_key(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.1", profile="ephemeral", chat=False)
    monkeypatch.setattr(main, "_ephemeral_sandbox_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)  # pre None -> create
    monkeypatch.setattr(main, "_create_sandbox", lambda n, u, s, p: sbx)
    ensure = MagicMock()
    monkeypatch.setattr(main, "_ensure_runtime_key", ensure)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    ensure.assert_called_once_with("c1")  # key minted BEFORE the pod is created


async def test_resolve_ephemeral_timeout_504(monkeypatch):
    sbx = make_sandbox(ready=False, profile="ephemeral", chat=False)
    monkeypatch.setattr(main, "_ephemeral_sandbox_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: None)
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert ei.value.status_code == 504


async def test_resolve_ephemeral_create_none_500(monkeypatch):
    monkeypatch.setattr(main, "_ephemeral_sandbox_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    monkeypatch.setattr(main, "_create_sandbox", lambda n, u, s, p: None)
    monkeypatch.setattr(main, "_ensure_runtime_key", MagicMock())
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert ei.value.status_code == 500


async def test_resolve_persistent_pre_existing_ready(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.3")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    name, ip = await main.resolve_sandbox("u1", "u1", main.PERSISTENT)  # session == user -> no migrate
    assert name == "cs1"
    assert ip == "10.0.0.3"


async def test_resolve_persistent_just_created_migrates(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.4")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)  # create
    monkeypatch.setattr(main, "_create_sandbox", lambda n, u, s, p: sbx)
    monkeypatch.setattr(main, "_ensure_runtime_key", MagicMock())
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    mig = AsyncMock()
    monkeypatch.setattr(main, "_migrate_staging_to_chat", mig)
    await main.resolve_sandbox("u1", "s1", main.PERSISTENT)  # session != user -> migrate
    mig.assert_awaited_once()


async def test_resolve_persistent_create_none_500(monkeypatch):
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    monkeypatch.setattr(main, "_create_sandbox", lambda n, u, s, p: None)
    monkeypatch.setattr(main, "_ensure_runtime_key", MagicMock())
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.PERSISTENT)
    assert ei.value.status_code == 500


async def test_resolve_persistent_timeout_504(monkeypatch):
    sbx = make_sandbox(ready=False)
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: None)
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "u1", main.PERSISTENT)
    assert ei.value.status_code == 504


# --- rotate-on-resume (issue #4) ------------------------------------------------
async def test_resolve_rotates_key_on_resume(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.5", operating_mode="Running")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)  # pre-existing
    # pre-existing + Suspended -> rotate-on-resume path
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    rotate = MagicMock()
    monkeypatch.setattr(main, "_rotate_runtime_key", rotate)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    await main.resolve_sandbox("u1", "u1", main.PERSISTENT)
    rotate.assert_called_once_with("cs1")


async def test_resolve_no_rotate_when_running(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.5")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    rotate = MagicMock()
    monkeypatch.setattr(main, "_rotate_runtime_key", rotate)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    await main.resolve_sandbox("u1", "u1", main.PERSISTENT)
    rotate.assert_not_called()


# --- _ensure_sandbox_running_ip --------------------------------------------------
async def test_ensure_running_ip_ready(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.6")
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    assert await main._ensure_sandbox_running_ip("s1") == "10.0.0.6"


async def test_ensure_running_ip_missing(monkeypatch):
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: None)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    assert await main._ensure_sandbox_running_ip("s1") is None


async def test_ensure_running_ip_rotates_when_suspended(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.7")
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    rotate = MagicMock()
    monkeypatch.setattr(main, "_rotate_runtime_key", rotate)
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: sbx)
    await main._ensure_sandbox_running_ip("s1")
    rotate.assert_called_once_with("s1")
