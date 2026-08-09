# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Endpoint tests — healthz, openapi, docs, api/config, api/status, proxy catch-all."""
from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import main  # type: ignore[import-not-found]

_AUTH = {"Authorization": "Bearer test-secret"}


def _resp(status=200, body=b"{}", headers=None, content_type="application/json"):
    r = MagicMock()
    r.status_code = status
    r.aread = AsyncMock(return_value=body)
    r.headers = headers if headers is not None else {"content-type": content_type}
    return r


def test_healthz(client):
    r = client.get("/healthz")
    assert r.status_code == 200
    assert r.json() == {"status": "ok"}


def test_openapi_json(client):
    r = client.get("/openapi.json")
    assert r.status_code == 200
    assert "openapi" in r.json() or "paths" in r.json() or "info" in r.json()


def test_docs(client):
    r = client.get("/docs")
    assert r.status_code == 200
    assert "swagger-ui" in r.text


def test_api_config_no_auth_401(client):
    assert client.get("/api/config").status_code == 401


def test_api_config_wrong_token_401(client):
    assert client.get("/api/config", headers={"Authorization": "Bearer wrong"}).status_code == 401


def test_api_config_valid(client):
    r = client.get("/api/config", headers=_AUTH)
    assert r.status_code == 200
    assert r.json()["features"]["terminal"]


def test_api_config_503_when_secret_unset(client, monkeypatch):
    # Deny-on-unset (defense-in-depth): _auth 503s when BROKER_SHARED_SECRET is unset —
    # fail-closed at the request path, independent of the startup boot guard.
    monkeypatch.setattr(main, "SHARED_SECRET", "")
    assert client.get("/api/config").status_code == 503


def test_api_config_503_when_secret_placeholder(client, monkeypatch):
    # Known placeholders are treated as unset (mirror _validate_config / _PLACEHOLDER_SECRETS).
    monkeypatch.setattr(main, "SHARED_SECRET", "placeholder")
    assert client.get("/api/config").status_code == 503


def test_api_status_valid(client):
    r = client.get("/api/status", headers=_AUTH)
    assert r.status_code == 200
    assert "active_pods" in r.json()


def test_proxy_missing_user_400(client):
    assert client.get("/execute", headers=_AUTH).status_code == 400


def test_proxy_success(client, httpx_client, monkeypatch):
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    httpx_client.send.return_value = _resp(200, b'{"ok":true}')
    r = client.post("/execute", headers={**_AUTH, "X-User-Id": "u1"}, json={"command": "echo hi"})
    assert r.status_code == 200
    assert r.content == b'{"ok":true}'


def test_proxy_redirect_location_rewrite(client, httpx_client, monkeypatch):
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    httpx_client.send.return_value = _resp(
        307, b"", headers={"location": "http://1.2.3.4:8888/files/list/", "content-type": "text/plain"})
    r = client.get("/files/list/", headers={**_AUTH, "X-User-Id": "u1"}, follow_redirects=False)
    assert r.status_code == 307
    assert r.headers["location"] == "/files/list/"


def test_proxy_injects_runtime_credential(client, httpx_client, monkeypatch):
    # The catch-all proxy strips the inbound Authorization (HOP) and injects the
    # PER-SESSION runtime Bearer for the target pod (issue #4: resolved from
    # owui-runtime-key-<sandbox>) so the broker -> router -> runtime hop clears
    # _auth_runtime on /execute, /files/* and the terminal management endpoints.
    monkeypatch.setattr(main, "resolve_sandbox", AsyncMock(return_value=("sbx-1", "10.0.0.1")))
    httpx_client.send.return_value = _resp(200, b"{}")
    client.post("/execute", headers={**_AUTH, "X-User-Id": "u1"}, json={"command": "echo hi"})
    sent = httpx_client.send.call_args.args[0]  # the forwarded httpx.Request
    assert sent.headers["authorization"] == f"Bearer {main._runtime_key_for('sbx-1')}"
