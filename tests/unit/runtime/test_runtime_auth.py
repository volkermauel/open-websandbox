# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Fail-closed PER-SESSION runtime API-key guard (issue #4).

The runtime no longer reads a shared ``RUNTIME_API_KEY`` from the environment. Each
sandbox pod gets its OWN broker<->runtime key, delivered as a projected Secret volume
mounted at ``/etc/runtime-key/api-key`` (the broker mints the key, writes Secret
``owui-runtime-key-<sandbox>``, and injects the volume into the per-session Sandbox
podTemplate — see chart + ``broker/main.py``). The runtime reads it from that FILE.

Covers:
  * ``_validate_runtime_config()`` startup guard: a missing/empty/placeholder key file
    must raise ``RuntimeError``; a strong key must pass.
  * ``_auth_runtime()`` request guard on the gated runtime surface. EVERY app-defined
    route except the two health/info endpoints (``GET /`` and ``GET /metrics``) is
    gated — ``POST /execute``, the full ``/files/*`` + ``/ports`` surface, the terminal
    management endpoints ``POST/GET/DELETE /api/terminals[/{id}]``, and the broker-backed
    LLM-tool surface ``/upload`` / ``/download`` / ``/list`` / ``/exists``. This invariant
    is enforced by ``test_full_surface_auth_invariant`` below: a route-table-driven
    regression guard that iterates ``app.routes`` and asserts each ``APIRoute`` 401s
    without a Bearer (except ``/`` + ``/metrics``, which stay 200) — so any newly-added
    ungated route fails CI. With a key set, a missing/wrong Bearer is 401 and a match is
    accepted; with the key UNSET the guard DENIES (503) — fail-closed, independent of the
    startup boot guard / lifespan. Rotate-on-resume (a freshly synced Secret) is honored
    via a cache-busting reload on mismatch. The WS auth-frame path is covered in
    test_terminal_extra.py; happy paths run via the default-Bearer ``client`` fixture.
"""

from __future__ import annotations

import re

import pytest
import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert
from fastapi.routing import APIRoute

_PLACEHOLDERS = ["", "dev-shared-secret-change-me", "change-me", "changeme", "placeholder"]


# --- startup guard -------------------------------------------------------------

@pytest.mark.parametrize("bad", _PLACEHOLDERS)
def test_validate_runtime_config_rejects_weak_key(runtime_key, bad):
    runtime_key.set(bad)
    with pytest.raises(RuntimeError):
        server._validate_runtime_config()


def test_validate_runtime_config_rejects_missing_file(runtime_key):
    runtime_key.unset()  # RUNTIME_KEY_FILE -> missing path
    with pytest.raises(RuntimeError):
        server._validate_runtime_config()


def test_validate_runtime_config_accepts_strong_key(runtime_key):
    runtime_key.set("a-very-strong-and-random-runtime-key-123456")
    server._validate_runtime_config()  # no raise


# --- request guard on the gated terminal surface ------------------------------

async def test_create_terminal_rejects_missing_bearer(workdir, client_noauth, runtime_key):
    runtime_key.set("s3cret-key")
    r = await client_noauth.post("/api/terminals", headers={"X-Session-Id": "nomissing"})
    assert r.status_code == 401
    assert "nomissing" not in server._terminals


async def test_create_terminal_rejects_wrong_bearer(workdir, client_noauth, runtime_key):
    runtime_key.set("s3cret-key")
    r = await client_noauth.post(
        "/api/terminals",
        headers={"X-Session-Id": "nowrong", "Authorization": "Bearer nope"},
    )
    assert r.status_code == 401
    assert "nowrong" not in server._terminals


async def test_create_terminal_accepts_correct_bearer(workdir, client_noauth, runtime_key):
    runtime_key.set("s3cret-key")
    r = await client_noauth.post(
        "/api/terminals",
        headers={"X-Session-Id": "ok", "Authorization": "Bearer s3cret-key"},
    )
    assert r.status_code == 200
    assert r.json()["id"] == "ok"
    server._term_cleanup("ok")


async def test_create_terminal_503_when_key_unset(workdir, client, runtime_key):
    # Deny-on-unset (defense-in-depth): with the per-session key file missing the request
    # guard 503s at the request path, independent of the startup boot guard / lifespan
    # (so a process that skipped the startup event still cannot serve a gated hop). Uses
    # the default-Bearer `client`; the credential is irrelevant — 503 fires before it.
    runtime_key.unset()
    r = await client.post("/api/terminals", headers={"X-Session-Id": "dev"})
    assert r.status_code == 503
    assert "dev" not in server._terminals


async def test_auth_runtime_reloads_on_rotate(workdir, client_noauth, runtime_key):
    # Rotate-on-resume: a cached key that has just been rotated (fresh Secret synced by the
    # kubelet -> new file mtime) must be honored WITHOUT a restart. We seed the cache with
    # the old key, rotate, and assert the NEW key is accepted (and the old one rejected) on
    # the very next request — the mismatch path cache-busts and re-reads the file.
    runtime_key.set("old-key")
    # populate the cache with the old value
    r = await client_noauth.post(
        "/api/terminals", headers={"X-Session-Id": "seed", "Authorization": "Bearer old-key"})
    assert r.status_code == 200
    server._term_cleanup("seed")
    new_key = runtime_key.rotate()
    # the old key must now be rejected ...
    r = await client_noauth.post(
        "/api/terminals", headers={"X-Session-Id": "old", "Authorization": "Bearer old-key"})
    assert r.status_code == 401
    # ... and the freshly rotated key accepted on the next request
    r = await client_noauth.post(
        "/api/terminals", headers={"X-Session-Id": "new", "Authorization": f"Bearer {new_key}"})
    assert r.status_code == 200
    server._term_cleanup("new")


# --- route-table-driven auth invariant (regression guard) ---------------------
# Instead of a hand-picked 4-route list, iterate `app.routes` and assert the FULL API
# surface: every app-defined route except the two health/info endpoints (GET / and
# GET /metrics) 401s without a Bearer. FastAPI's framework-generated /docs, /redoc,
# /openapi.json are plain `starlette.routing.Route` (not APIRoute) and the WS terminal
# route is an APIWebSocketRoute, so filtering to APIRoute covers exactly the app's own
# endpoints. Any newly-added ungated route fails this test on purpose.

_OPEN = {"/", "/metrics"}


def _surface_cases() -> list:
    """Build (method, path, expected_status) cases from the live route table.

    Path params are filled with a placeholder (``{file_path:path}`` -> ``x``) so the
    request matches the route; the auth dependency fires during dependency resolution,
    before any path/query/body validation, so a 401 short-circuits regardless of the
    placeholder values. Non-GET requests carry an empty JSON body for the same reason.
    """
    cases = []
    for route in server.app.routes:
        if not isinstance(route, APIRoute):
            continue  # skip WebSocket + framework doc/schema routes
        method = next(iter(sorted(route.methods - {"HEAD"})))
        path = re.sub(r"\{[^}]+\}", "x", route.path)
        expected = 200 if route.path in _OPEN else 401
        cases.append(pytest.param(method, path, expected, id=f"{method} {route.path}"))
    return cases


@pytest.mark.parametrize("method,path,expected", _surface_cases())
async def test_full_surface_auth_invariant(
    workdir, client_noauth, runtime_key, method, path, expected
):
    # With a key set, EVERY gated endpoint rejects a missing Bearer with 401; the two
    # health/info endpoints (/, /metrics) stay 200 — proving the whole surface (not just
    # a hand-picked subset) is wired to _auth_runtime, and that / + /metrics are the
    # only intentionally-open routes.
    runtime_key.set("s3cret-key")
    kwargs = {"json": {}} if method != "GET" else {}
    r = await client_noauth.request(method, path, **kwargs)
    assert r.status_code == expected, (
        f"{method} {path}: expected {expected}, got {r.status_code} [{r.text[:120]}]"
    )
