# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Extra ``/execute`` + module-helper coverage.

Covers the branches the happy-path suite in ``test_execute.py`` misses:
  * ``server._env_int`` swallowing a malformed int env var (the ``except``).
  * ``POST /execute`` hitting the outer ``except OSError`` (runtime error →
    exit_code 1). We do this WITHOUT mocking the subprocess: the real
    ``create_subprocess_shell`` raises ``NotADirectoryError`` (an ``OSError``)
    when ``cwd`` points at a file instead of a directory, so we point WORKDIR
    at a real file and let the real spawn fail.
"""

from __future__ import annotations

import httpx
import pytest
import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert


def test_env_int_falls_back_on_garbage(monkeypatch: pytest.MonkeyPatch):
    # A non-numeric value must hit the (TypeError, ValueError) fallback.
    monkeypatch.setenv("NLBL_BAD_INT", "not-a-number")
    assert server._env_int("NLBL_BAD_INT", 42) == 42


def test_env_int_uses_value_when_valid(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("NLBL_GOOD_INT", "7")
    assert server._env_int("NLBL_GOOD_INT", 42) == 7


def test_env_int_missing_uses_default(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv("NLBL_MISSING_INT", raising=False)
    assert server._env_int("NLBL_MISSING_INT", 13) == 13


async def test_execute_runtime_oserror(tmp_path, monkeypatch: pytest.MonkeyPatch):
    # Point WORKDIR at a real FILE: the real subprocess spawn then raises
    # NotADirectoryError (OSError) → the /execute OSError branch returns
    # exit_code 1 with a "runtime error: ..." stderr. No subprocess mocking.
    not_a_dir = tmp_path / "isafile"
    not_a_dir.write_text("x")
    monkeypatch.setattr(server, "WORKDIR", str(not_a_dir))

    transport = httpx.ASGITransport(app=server.app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test", headers={"Authorization": f"Bearer {server._load_session_key()}"}) as c:
        r = await c.post("/execute", json={"command": "true"})
    assert r.status_code == 200
    body = r.json()
    assert body["exit_code"] == 1
    assert not body["timed_out"]
    assert "runtime error" in body["stderr"]
