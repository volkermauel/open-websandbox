"""Migrate tests — staging->chat file move, clear, and every failure branch."""
from __future__ import annotations

from unittest.mock import MagicMock

import main  # type: ignore[import-not-found]
from conftest import api_exc, make_sandbox


def _resp(status=200, json_data=None, content=b""):
    r = MagicMock()
    r.status_code = status
    r.content = content
    r.json.return_value = json_data if json_data is not None else {}
    return r


def _stage(monkeypatch, sandbox=None, sip: str | None = "sip"):
    """Wire _get_sandbox + _ensure_sandbox_running_ip for a reachable staging pod."""
    monkeypatch.setattr(main, "_get_sandbox", lambda n: (sandbox if sandbox is not None else make_sandbox()))
    monkeypatch.setattr(main, "_ensure_sandbox_running_ip", _async_const(sip))


def _async_const(value):
    from unittest.mock import AsyncMock
    return AsyncMock(return_value=value)


async def test_migrate_staging_absent_returns(monkeypatch, fresh_migrate_locks):
    monkeypatch.setattr(main, "_get_sandbox", lambda n: None)
    await main._migrate_staging_to_chat("u1", "10.0.0.1")  # early return


async def test_migrate_staging_not_reachable_deletes(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch, sip=None)
    deleted = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await main._migrate_staging_to_chat("u1", "10.0.0.1")  # unreachable -> delete staging
    assert deleted == [main._chat_sandbox_name("u1", "u1")]


async def test_migrate_success(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(200, {"entries": [{"name": "a"}, {"name": "b"}]})
    httpx_client.post.side_effect = [_resp(200, content=b"ZIP"), _resp(200), _resp(200), _resp(200)]
    await main._migrate_staging_to_chat("u1", "chatip")
    assert httpx_client.post.call_count == 4  # archive, upload, extract, clear


async def test_migrate_list_empty(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(200, {"entries": []})
    httpx_client.post.return_value = _resp(200)  # clear only
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_migrate_list_non200(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(500)
    httpx_client.post.return_value = _resp(200)  # clear only
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_migrate_archive_fail(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(200, {"entries": [{"name": "a"}]})
    httpx_client.post.side_effect = [_resp(500), _resp(200)]  # archive fail, clear
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_migrate_archive_no_content(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(200, {"entries": [{"name": "a"}]})
    httpx_client.post.side_effect = [_resp(200, content=b""), _resp(200)]  # archive 200 empty, clear
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_migrate_upload_fail(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.return_value = _resp(200, {"entries": [{"name": "a"}]})
    httpx_client.post.side_effect = [_resp(200, content=b"Z"), _resp(500), _resp(200)]  # archive, upload fail, clear
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_migrate_move_phase_exception_still_clears(monkeypatch, fresh_migrate_locks, httpx_client):
    _stage(monkeypatch)
    httpx_client.get.side_effect = Exception("boom")  # list raises -> inner except -> clear still runs
    httpx_client.post.return_value = _resp(200)
    await main._migrate_staging_to_chat("u1", "chatip")
    httpx_client.post.assert_called()  # clear ran despite the move-phase failure


async def test_migrate_outer_exception(monkeypatch, fresh_migrate_locks):
    def boom(n):
        raise api_exc(500)
    monkeypatch.setattr(main, "_get_sandbox", boom)  # non-404 -> outer except, non-fatal
    await main._migrate_staging_to_chat("u1", "chatip")


async def test_clear_workspace_suppresses_errors(httpx_client):
    httpx_client.post.side_effect = Exception("down")
    await main._clear_workspace("1.2.3.4")  # suppressed -> no raise
