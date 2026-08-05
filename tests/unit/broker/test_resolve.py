"""Resolve tests — sandbox get-or-create, resume parked, migrate trigger, timeout, vanish."""
from __future__ import annotations

from unittest.mock import AsyncMock

import pytest

import main  # type: ignore[import-not-found]
from conftest import make_claim, make_sandbox


# --- resolve_sandbox (ephemeral) -------------------------------------------------
async def test_resolve_ephemeral_ready_first_iter(monkeypatch):
    claim = make_claim(ready=True, sandbox="sbx-1", pod_ip="10.0.0.1")
    monkeypatch.setattr(main, "_claim_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_claim", lambda n: claim)
    monkeypatch.setattr(main, "_create_claim", lambda n, p: claim)
    monkeypatch.setattr(main, "_touch", lambda n: None)
    sbx, ip = await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert sbx == "sbx-1"
    assert ip == "10.0.0.1"


async def test_resolve_ephemeral_deadline_504(monkeypatch):
    claim = make_claim(ready=False, sandbox="sbx-1", pod_ip="10.0.0.1")
    monkeypatch.setattr(main, "_claim_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_claim", lambda n: claim)
    monkeypatch.setattr(main, "_create_claim", lambda n, p: claim)
    monkeypatch.setattr(main, "CLAIM_READY_TIMEOUT", -10)
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert ei.value.status_code == 504


async def test_resolve_ephemeral_vanish_500(monkeypatch, no_sleep):
    not_ready = make_claim(ready=False)
    monkeypatch.setattr(main, "_claim_name", lambda u, s: "c1")
    results = iter([not_ready, None])
    monkeypatch.setattr(main, "_get_claim", lambda n: next(results))
    monkeypatch.setattr(main, "_create_claim", lambda n, p: not_ready)
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert ei.value.status_code == 500


# --- resolve_sandbox (persistent delegates) -------------------------------------
async def test_resolve_persistent_delegates_to_chat(monkeypatch):
    chat = AsyncMock(return_value=("chat-sbx", "10.0.0.2"))
    monkeypatch.setattr(main, "_resolve_chat_sandbox", chat)
    sbx, ip = await main.resolve_sandbox("u1", "s1", main.PERSISTENT)
    assert sbx == "chat-sbx"
    assert ip == "10.0.0.2"
    chat.assert_awaited_with("u1", "s1")


# --- _resolve_chat_sandbox -------------------------------------------------------
async def test_chat_pre_existing_ready_return(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.3")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    name, ip = await main._resolve_chat_sandbox("u1", "u1")  # session == user -> no migrate
    assert name == "cs1"
    assert ip == "10.0.0.3"


async def test_chat_just_created_migrates(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.4")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)  # pre None -> create
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    mig = AsyncMock()
    monkeypatch.setattr(main, "_migrate_staging_to_chat", mig)
    await main._resolve_chat_sandbox("u1", "s1")  # session != user
    mig.assert_awaited_once()


async def test_chat_create_returns_none_500(monkeypatch):
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: None)
    with pytest.raises(main.HTTPException) as ei:
        await main._resolve_chat_sandbox("u1", "s1")
    assert ei.value.status_code == 500


async def test_chat_suspended_resumes(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.5")
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    resumed = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: resumed.append((n, m)))
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    await main._resolve_chat_sandbox("u1", "u1")
    expected = ("sbx-1", "Running")
    assert expected in resumed


async def test_chat_deadline_504(monkeypatch):
    sbx = make_sandbox(ready=False)
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "CLAIM_READY_TIMEOUT", -10)
    with pytest.raises(main.HTTPException) as ei:
        await main._resolve_chat_sandbox("u1", "u1")
    assert ei.value.status_code == 504


async def test_chat_vanish_500(monkeypatch, no_sleep):
    not_ready = make_sandbox(ready=False)
    monkeypatch.setattr(main, "_chat_sandbox_name", lambda u, s: "cs1")
    results = iter([not_ready, None])
    monkeypatch.setattr(main, "_get_sandbox", lambda n: next(results))
    monkeypatch.setattr(main, "_create_chat_sandbox", lambda n, u, s: not_ready)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    with pytest.raises(main.HTTPException) as ei:
        await main._resolve_chat_sandbox("u1", "u1")
    assert ei.value.status_code == 500


# --- _ensure_sandbox_running_ip --------------------------------------------------
async def test_ensure_running_ip_ready(monkeypatch):
    sbx = make_sandbox(ready=True, pod_ip="10.0.0.6")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    assert await main._ensure_sandbox_running_ip("s1") == "10.0.0.6"


async def test_ensure_running_ip_missing(monkeypatch):
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    assert await main._ensure_sandbox_running_ip("s1") is None


async def test_ensure_running_ip_suspended_then_timeout(monkeypatch, no_sleep):
    sbx = make_sandbox(ready=False)
    monkeypatch.setattr(main, "_get_sandbox", lambda n: sbx)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Suspended")
    resumed = []
    monkeypatch.setattr(main, "_set_sandbox_operating_mode", lambda n, m: resumed.append((n, m)))
    ticks = iter([1000.0, 1000.0, 2000.0])  # deadline, enter-loop, exit-loop
    monkeypatch.setattr(main.time, "time", lambda: next(ticks))
    assert await main._ensure_sandbox_running_ip("s1", timeout=0.5) is None
    expected_pair = ("s1", "Running")
    assert expected_pair in resumed


# --- resolve_sandbox ephemeral: claim could not be created ----------------------
async def test_resolve_ephemeral_claim_none_500(monkeypatch):
    monkeypatch.setattr(main, "_claim_name", lambda u, s: "c1")
    monkeypatch.setattr(main, "_get_claim", lambda n: None)
    monkeypatch.setattr(main, "_create_claim", lambda n, p: None)
    with pytest.raises(main.HTTPException) as ei:
        await main.resolve_sandbox("u1", "s1", main.EPHEMERAL)
    assert ei.value.status_code == 500
