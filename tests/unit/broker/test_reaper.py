# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Reaper tests — park/reap persistent + ephemeral per-session Sandboxes (issue #4).

All broker-owned sandboxes are direct `agents.x-k8s.io/Sandbox` objects (the
SandboxClaim warm-pool path is gone); the reaper selects on managed-by=owui-broker,
parks persistent sandboxes at PARK_TTL, reaps persistent at REAP_TTL / ephemeral at
IDLE_TTL, and reaps the per-session runtime-key Secret with the sandbox."""
from __future__ import annotations

import asyncio
import time
from unittest.mock import AsyncMock

import main  # type: ignore[import-not-found]
import pytest
from conftest import make_sandbox


@pytest.fixture
def reaper_one_tick(monkeypatch):
    """Patch asyncio.sleep to cancel the reaper after its first full iteration."""
    async def _break(*_a, **_k):
        raise asyncio.CancelledError()
    monkeypatch.setattr(main.asyncio, "sleep", _break)


async def _run_once():
    task = asyncio.create_task(main._reaper_loop())
    try:
        await task
    except BaseException:  # CancelledError after one tick, or any test-setup error
        pass


async def test_reaper_park_persistent(api, monkeypatch, reaper_one_tick):
    old = int(time.time() - main.PARK_TTL - 5)
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=old)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    parked = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    await _run_once()
    assert ("owui-c-1", "Suspended") in parked


async def test_reaper_reap_persistent(api, monkeypatch, reaper_one_tick):
    old = int(time.time() - main.REAP_TTL - 5)
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=old)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: None)
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert "owui-c-1" in deleted


async def test_reaper_reap_ephemeral(api, monkeypatch, reaper_one_tick):
    old = int(time.time() - main.IDLE_TTL - 5)
    sbx = make_sandbox(name="owui-1", profile="ephemeral", last_used=old, chat=False)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert "owui-1" in deleted


async def test_reaper_ephemeral_not_idle_kept(api, monkeypatch, reaper_one_tick):
    monkeypatch.setattr(main, "IDLE_TTL", 10 ** 18)  # idle never exceeds TTL -> kept
    sbx = make_sandbox(name="owui-1", profile="ephemeral", last_used=1, chat=False)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert deleted == []


async def test_reaper_park_idempotent_when_already_suspended(api, monkeypatch, reaper_one_tick):
    old = int(time.time() - main.PARK_TTL - 5)
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=old)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    parked = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    await _run_once()
    assert parked == []  # already Suspended -> not re-parked


async def test_reaper_skips_sandbox_with_no_last_used(api, monkeypatch, reaper_one_tick):
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=0)
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert deleted == []


def test_delete_sandbox_calls_api_and_reaps_key(api, monkeypatch):
    reaped = []
    monkeypatch.setattr(main, "_delete_runtime_key", lambda n: reaped.append(n))
    main._delete_sandbox("sbx-1")
    api.delete_namespaced_custom_object.assert_called_once()
    assert reaped == ["sbx-1"]  # per-session key Secret reaped with the sandbox


async def test_start_reaper_creates_task(monkeypatch):
    loop = AsyncMock()
    monkeypatch.setattr(main, "_reaper_loop", loop)
    monkeypatch.setattr(main, "_acquire_or_renew_lease", lambda: True)  # we win the lease
    await main._start_reaper()
    await asyncio.sleep(0.05)  # let the leader loop's first iteration run the reaper
    loop.assert_awaited()
    main._leader_task.cancel()
    try:
        await main._leader_task
    except BaseException:
        pass
