"""Leader election — only the lease-holding broker replica runs the reaper."""
from __future__ import annotations

import asyncio
import contextlib
import datetime
from unittest.mock import MagicMock

import main  # type: ignore[import-not-found]


def _lease(holder, renew, duration=15):
    """A fake V1Lease with a populated spec."""
    lease = MagicMock()
    lease.spec = MagicMock()
    lease.spec.holder_identity = holder
    lease.spec.renew_time = renew
    lease.spec.lease_duration_seconds = duration
    return lease


def _now():
    return datetime.datetime.now(datetime.timezone.utc)


def test_lease_create_when_absent(monkeypatch):
    coord = MagicMock()
    coord.read_namespaced_lease.side_effect = main.client.ApiException(status=404)
    monkeypatch.setattr(main, "_coord", lambda: coord)
    assert main._acquire_or_renew_lease()
    coord.create_namespaced_lease.assert_called_once()


def test_lease_renew_when_ours(monkeypatch):
    coord = MagicMock()
    coord.read_namespaced_lease.return_value = _lease(main._LEADER_IDENTITY, _now())
    monkeypatch.setattr(main, "_coord", lambda: coord)
    assert main._acquire_or_renew_lease()
    coord.replace_namespaced_lease.assert_called_once()


def test_lease_defer_when_other_live(monkeypatch):
    coord = MagicMock()
    coord.read_namespaced_lease.return_value = _lease("broker-other", _now(), duration=60)
    monkeypatch.setattr(main, "_coord", lambda: coord)
    assert not main._acquire_or_renew_lease()
    coord.replace_namespaced_lease.assert_not_called()


def test_lease_takeover_when_expired(monkeypatch):
    coord = MagicMock()
    old = _now() - datetime.timedelta(seconds=999)
    coord.read_namespaced_lease.return_value = _lease("broker-other", old, duration=15)
    monkeypatch.setattr(main, "_coord", lambda: coord)
    assert main._acquire_or_renew_lease()
    coord.replace_namespaced_lease.assert_called_once()


def test_lease_handles_missing_spec(monkeypatch):
    # lease exists but spec is None -> fall back to a fresh V1LeaseSpec (holder None -> we take over)
    coord = MagicMock()
    lease = MagicMock()
    lease.spec = None
    coord.read_namespaced_lease.return_value = lease
    monkeypatch.setattr(main, "_coord", lambda: coord)
    assert main._acquire_or_renew_lease()
    coord.replace_namespaced_lease.assert_called_once()


async def test_apply_leadership_starts_then_stops_reaper(monkeypatch):
    main._reaper_task = None
    ran = []

    async def fake_reaper():
        ran.append(True)
        await asyncio.sleep(100)  # stays running so the stop branch cancels it

    monkeypatch.setattr(main, "_reaper_loop", fake_reaper)
    await main._apply_leadership(True)
    assert main._reaper_task is not None
    await asyncio.sleep(0.05)
    assert ran  # reaper ran
    await main._apply_leadership(False)  # lose leadership -> cancel + clear
    assert main._reaper_task is None
    await main._apply_leadership(False)  # no-op when already stopped


async def test_leader_loop_gates_apply_on_lease(monkeypatch):
    results = iter([True, False])
    monkeypatch.setattr(main, "_acquire_or_renew_lease", lambda: next(results))
    applied = []

    async def fake_apply(leader):
        applied.append(leader)

    monkeypatch.setattr(main, "_apply_leadership", fake_apply)
    n = [0]

    async def sleep_then_cancel(_s):
        n[0] += 1
        if n[0] >= 2:
            raise asyncio.CancelledError()

    monkeypatch.setattr(main.asyncio, "sleep", sleep_then_cancel)
    with contextlib.suppress(asyncio.CancelledError):
        await main._leader_loop()
    assert applied == [True, False]


def test_coord_lazy_init_caches(monkeypatch):
    monkeypatch.setattr(main, "_coord_api", None)
    fake = MagicMock()
    monkeypatch.setattr(main.client, "CoordinationV1Api", lambda: fake)
    assert main._coord() is fake  # created on first call
    assert main._coord() is fake  # cached on second
