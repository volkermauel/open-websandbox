"""Fail-closed RUNTIME_API_KEY guard — mirrors the broker's BROKER_SHARED_SECRET story.

Covers:
  * ``_validate_runtime_config()`` startup guard: unset + each known placeholder must
    raise ``RuntimeError``; a strong key must pass (mirrors broker
    ``test_validate_config_*`` in tests/unit/broker/test_coverage_gaps.py).
  * ``_auth_runtime()`` request guard on ``POST /api/terminals`` (the one runtime hop the
    broker authenticates with a Bearer): with a key set, a missing or wrong Bearer is
    rejected (401) and a matching Bearer is accepted; with the key unset the guard is a
    no-op (dev/tests), mirroring ``broker._auth``. The boot guard is what makes this
    fail-closed in production (refuses to start without a key).

The HTTP hops the broker reaches WITHOUT a credential today (``POST /execute``,
``/files/*``) are intentionally NOT gated yet (the broker attaches no Bearer there —
tracked in #19); the WS auth-frame path is covered in test_terminal_extra.py.
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


# --- request guard on POST /api/terminals (the broker-authenticated hop) ------

async def test_create_terminal_rejects_missing_bearer(workdir, client, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client.post("/api/terminals", headers={"X-Session-Id": "nomissing"})
    assert r.status_code == 401
    assert "nomissing" not in server._terminals


async def test_create_terminal_rejects_wrong_bearer(workdir, client, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client.post(
        "/api/terminals",
        headers={"X-Session-Id": "nowrong", "Authorization": "Bearer nope"},
    )
    assert r.status_code == 401
    assert "nowrong" not in server._terminals


async def test_create_terminal_accepts_correct_bearer(workdir, client, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    r = await client.post(
        "/api/terminals",
        headers={"X-Session-Id": "ok", "Authorization": "Bearer s3cret-key"},
    )
    assert r.status_code == 200
    assert r.json()["id"] == "ok"
    server._term_cleanup("ok")


async def test_create_terminal_open_when_key_unset(workdir, client, monkeypatch):
    # Dev/test no-op: with RUNTIME_API_KEY unset the request guard is disabled (mirrors
    # broker._auth). _validate_runtime_config() is what makes this fail-closed in
    # production (it refuses to boot on an unset/placeholder key).
    monkeypatch.delenv("RUNTIME_API_KEY", raising=False)
    r = await client.post("/api/terminals", headers={"X-Session-Id": "dev"})
    assert r.status_code == 200
    server._term_cleanup("dev")
