"""Fail-closed RUNTIME_API_KEY guard — mirrors the broker's BROKER_SHARED_SECRET story.

Covers:
  * ``_validate_runtime_config()`` startup guard: unset + each known placeholder must
    raise ``RuntimeError``; a strong key must pass (mirrors broker
    ``test_validate_config_*`` in tests/unit/broker/test_coverage_gaps.py).
  * ``_auth_runtime()`` request guard on the gated runtime surface (POST /execute,
    /files/* and the terminal management endpoints POST/GET/DELETE /api/terminals[/{id}]):
    with a key set, a missing or wrong Bearer is rejected (401) and a matching Bearer is
    accepted; with the key UNSET the guard DENIES (503) — fail-closed at the request path,
    independent of the startup boot guard / lifespan. The WS auth-frame path is covered in
    test_terminal_extra.py; the gated /files/* + /execute surface is exercised across the
    suite via the default-Bearer ``client`` fixture (see conftest.RT_AUTH).
"""

from __future__ import annotations

import pytest
import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert

_PLACEHOLDERS = ["", "dev-shared-secret-change-me", "change-me", "changeme", "placeholder"]


# --- startup guard -------------------------------------------------------------

@pytest.mark.parametrize("bad", _PLACEHOLDERS)
def test_validate_runtime_config_rejects_weak_key(monkeypatch, bad):
    monkeypatch.setenv("RUNTIME_API_KEY", bad)
    with pytest.raises(RuntimeError):
        server._validate_runtime_config()


def test_validate_runtime_config_accepts_strong_key(monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "a-very-strong-and-random-runtime-key-123456")
    server._validate_runtime_config()  # no raise


# --- request guard on the gated terminal surface ------------------------------

async def test_create_terminal_rejects_missing_bearer(workdir, client_noauth, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client_noauth.post("/api/terminals", headers={"X-Session-Id": "nomissing"})
    assert r.status_code == 401
    assert "nomissing" not in server._terminals


async def test_create_terminal_rejects_wrong_bearer(workdir, client_noauth, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client_noauth.post(
        "/api/terminals",
        headers={"X-Session-Id": "nowrong", "Authorization": "Bearer nope"},
    )
    assert r.status_code == 401
    assert "nowrong" not in server._terminals


async def test_create_terminal_accepts_correct_bearer(workdir, client_noauth, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client_noauth.post(
        "/api/terminals",
        headers={"X-Session-Id": "ok", "Authorization": "Bearer s3cret-key"},
    )
    assert r.status_code == 200
    assert r.json()["id"] == "ok"
    server._term_cleanup("ok")


async def test_create_terminal_503_when_key_unset(workdir, client, monkeypatch):
    # Deny-on-unset (defense-in-depth): with RUNTIME_API_KEY unset/placeholder the request
    # guard 503s at the request path, independent of the startup boot guard / lifespan
    # (so a process that skipped the startup event still cannot serve a gated hop). Uses
    # the default-Bearer `client`; the credential is irrelevant — 503 fires before it.
    monkeypatch.delenv("RUNTIME_API_KEY", raising=False)
    r = await client.post("/api/terminals", headers={"X-Session-Id": "dev"})
    assert r.status_code == 503
    assert "dev" not in server._terminals


# --- the rest of the gated surface 401s without the Bearer --------------------

@pytest.mark.parametrize("method,path,kwargs", [
    ("POST", "/execute", {"json": {"command": "true"}}),
    ("GET", "/files/list", {"params": {"directory": "/workspace"}}),
    ("GET", "/files/cwd", {}),
    ("GET", "/api/terminals", {}),
])
async def test_gated_surface_rejects_missing_bearer(workdir, client_noauth, monkeypatch, method, path, kwargs):
    # With a key set, EVERY gated endpoint rejects a missing Bearer with 401 — proves the
    # whole surface (not just POST /api/terminals) is wired to _auth_runtime.
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await getattr(client_noauth, method.lower())(path, **kwargs)
    assert r.status_code == 401
