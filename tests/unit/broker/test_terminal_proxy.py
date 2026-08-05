"""Terminal WS tests — auth, missing identity, resolve fail, success proxy, upstream fail."""
from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
from starlette.websockets import WebSocketDisconnect

import main  # type: ignore[import-not-found]


def test_terminal_missing_user_1008(client, monkeypatch):
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    with client.websocket_connect("/api/terminals/sess") as ws:
        with pytest.raises(WebSocketDisconnect) as ei:
            ws.receive_text()
        assert ei.value.code == 1008


def test_terminal_invalid_token_4001(client):
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        ws.send_text('{"type": "auth", "token": "wrong"}')
        with pytest.raises(WebSocketDisconnect) as ei:
            ws.receive_text()
        assert ei.value.code == 4001


def test_terminal_bad_json_4001(client):
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        ws.send_text("not-json{")
        with pytest.raises(WebSocketDisconnect) as ei:
            ws.receive_text()
        assert ei.value.code == 4001


def test_terminal_resolve_fail_1011(client, monkeypatch):
    monkeypatch.setattr(main, "SHARED_SECRET", "")

    async def boom(*a, **k):
        raise main.HTTPException(504, "nope")

    monkeypatch.setattr(main, "resolve_sandbox", boom)
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        with pytest.raises(WebSocketDisconnect) as ei:
            ws.receive_text()
        assert ei.value.code == 1011


def test_terminal_upstream_connect_fail_1011(client, monkeypatch, httpx_client):
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))

    def boom(*a, **k):
        raise ConnectionRefusedError("upstream down")

    monkeypatch.setattr(main, "websockets", SimpleNamespace(connect=boom))
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        with pytest.raises(WebSocketDisconnect) as ei:
            ws.receive_text()
        assert ei.value.code == 1011


def test_terminal_success_no_auth(client, monkeypatch, httpx_client, patch_websockets):
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    up = patch_websockets([b"hello-upstream", "text-msg"])
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        assert ws.receive_bytes() == b"hello-upstream"
        assert ws.receive_text() == "text-msg"
        ws.send_bytes(b"client-bytes")
        ws.send_text("client-text")
    assert b"client-bytes" in up.sent
    assert "client-text" in up.sent


def test_terminal_valid_auth_then_success(client, monkeypatch, httpx_client, patch_websockets):
    # SHARED_SECRET stays "test-secret"; valid first-message auth unlocks the proxy.
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    patch_websockets([b"post-auth-msg"])
    with client.websocket_connect("/api/terminals/sess?user_id=u&session_id=s") as ws:
        ws.send_text('{"type": "auth", "token": "test-secret"}')
        assert ws.receive_bytes() == b"post-auth-msg"
