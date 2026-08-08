"""Reaper tests — park/reap persistent + ephemeral claims, chat sandboxes, lifecycle."""
from __future__ import annotations

import asyncio
import time
from unittest.mock import AsyncMock

import main  # type: ignore[import-not-found]
import pytest
from conftest import make_claim, make_sandbox


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
    now = time.time()
    old = int(now - main.PARK_TTL - 5)
    claim = make_claim(name="c1", profile="persistent", last_used=old, sandbox="sbx-1")
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    parked = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    monkeypatch.setattr(main, "_delete_claim", lambda n: None)
    await _run_once()
    expected = ("sbx-1", "Suspended")
    assert expected in parked


async def test_reaper_reap_persistent(api, monkeypatch, reaper_one_tick):
    now = time.time()
    old = int(now - main.REAP_TTL - 5)
    claim = make_claim(name="c1", profile="persistent", last_used=old, sandbox="sbx-1")
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: None)
    deleted = []
    monkeypatch.setattr(main, "_delete_claim", lambda n: deleted.append(n))
    await _run_once()
    assert "c1" in deleted


async def test_reaper_reap_ephemeral(api, monkeypatch, reaper_one_tick):
    now = time.time()
    old = int(now - main.IDLE_TTL - 5)
    claim = make_claim(name="c1", profile="ephemeral", last_used=old, sandbox=None)
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    deleted = []
    monkeypatch.setattr(main, "_delete_claim", lambda n: deleted.append(n))
    await _run_once()
    assert "c1" in deleted


async def test_reaper_ephemeral_not_idle_kept(api, monkeypatch, reaper_one_tick):
    monkeypatch.setattr(main, "IDLE_TTL", 10**18)  # idle never exceeds TTL -> kept
    claim = make_claim(name="c1", profile="ephemeral", last_used=1, sandbox=None)
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    deleted = []
    monkeypatch.setattr(main, "_delete_claim", lambda n: deleted.append(n))
    await _run_once()
    assert deleted == []


async def test_reaper_park_chat_sandbox(api, monkeypatch, reaper_one_tick):
    now = time.time()
    old = int(now - main.PARK_TTL - 5)
    sbx = make_sandbox(name="owui-c-1", last_used=old)
    api.list_namespaced_custom_object.side_effect = [{"items": []}, {"items": [sbx]}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    parked = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: parked.append((n, m)))
    await _run_once()
    expected = ("owui-c-1", "Suspended")
    assert expected in parked


async def test_reaper_reap_chat_sandbox(api, monkeypatch, reaper_one_tick):
    now = time.time()
    old = int(now - main.REAP_TTL - 5)
    sbx = make_sandbox(name="owui-c-1", last_used=old)
    api.list_namespaced_custom_object.side_effect = [{"items": []}, {"items": [sbx]}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: None)
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert "owui-c-1" in deleted


def test_delete_claim_calls_api(api):
    main._delete_claim("c1")  # success path (ApiException arm is pragma:no-cover)
    api.delete_namespaced_custom_object.assert_called_once()


def test_delete_sandbox_calls_api(api):
    main._delete_sandbox("sbx-1")
    api.delete_namespaced_custom_object.assert_called_once()


async def test_start_reaper_creates_task(monkeypatch):
    # _start_reaper now starts the leader loop, which runs the reaper only when we lead.
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


async def test_reaper_skips_claim_with_no_last_used(api, monkeypatch, reaper_one_tick):
    claim = make_claim(name="c1", profile="ephemeral", last_used=0, sandbox=None)
    api.list_namespaced_custom_object.side_effect = [{"items": [claim]}, {"items": []}]
    deleted = []
    monkeypatch.setattr(main, "_delete_claim", lambda n: deleted.append(n))
    await _run_once()
    assert deleted == []


async def test_reaper_skips_sandbox_with_no_last_used(api, monkeypatch, reaper_one_tick):
    sbx = make_sandbox(name="owui-c-1", last_used=0)
    api.list_namespaced_custom_object.side_effect = [{"items": []}, {"items": [sbx]}]
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _run_once()
    assert deleted == []
