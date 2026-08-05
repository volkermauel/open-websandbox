"""Path-confinement tests for ``server._safe_path`` / ``_request_base``.

This is the security-critical boundary of the runtime: every file endpoint funnels
through ``_safe_path(rel, base)``, which must reject ANY path that resolves
outside the workspace base. We throw the usual traversal arsenal at it directly
(function level) and again through the HTTP endpoints (integration level) to
confirm escapes come back as HTTP 400 rather than 200-with-leaked-bytes.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from fastapi import HTTPException

import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert


# --- helpers ------------------------------------------------------------------

def assert_escape(rel: str, base: str) -> None:
    """A path that must be rejected by _safe_path with a 400."""
    with pytest.raises(HTTPException) as exc:
        server._safe_path(rel, base)
    assert exc.value.status_code == 400, f"{rel!r} should be rejected (got {exc.value.status_code})"


def assert_within(rel: str, base: str) -> str:
    """A path that is allowed; assert it resolves inside base and return it."""
    full = server._safe_path(rel, base)
    assert full == base or full.startswith(base + os.sep), f"{rel!r} escaped base: {full}"
    return full


# --- direct function tests ----------------------------------------------------

def test_safe_path_rejects_dotdot_traversal(workdir: str):
    assert_escape("../../etc/passwd", workdir)
    assert_escape("../../../etc/passwd", workdir)


def test_safe_path_rejects_absolute_outside(workdir: str):
    assert_escape("/etc/passwd", workdir)
    assert_escape("/etc", workdir)
    assert_escape("/root/.ssh/id_rsa", workdir)


def test_safe_path_rejects_url_encoded_traversal(workdir: str):
    # %2e%2e == "..", %2f == "/" — must be decoded first, then confined.
    assert_escape("%2e%2e/%2e%2e/etc/passwd", workdir)
    assert_escape("%2e%2e%2f%2e%2e%2fetc%2fpasswd", workdir)


def test_safe_path_rejects_url_encoded_absolute(workdir: str):
    # %2fetc%2fpasswd -> /etc/passwd after unquote -> absolute escape.
    assert_escape("%2fetc%2fpasswd", workdir)


def test_safe_path_rejects_symlink_escape(workdir: str):
    # A symlink living *inside* base that points *outside* must not let reads leak.
    link = os.path.join(workdir, "etc_link")
    os.symlink("/etc", link)
    assert_escape("etc_link", workdir)
    assert_escape("etc_link/passwd", workdir)
    # ... but a symlink to somewhere still inside base is fine.
    inner_target = os.path.join(workdir, "real")
    Path(inner_target).mkdir()
    good_link = os.path.join(workdir, "good_link")
    os.symlink(inner_target, good_link)
    assert_within("good_link", workdir)


def test_safe_path_accepts_legitimate_relative(workdir: str):
    assert_within("foo.txt", workdir)
    assert_within("a/b/c.txt", workdir)
    # internal ".." normalisation that stays inside base is allowed.
    assert os.path.realpath(os.path.join(workdir, "bar")) == assert_within("a/../bar", workdir)


def test_safe_path_accepts_base_itself(workdir: str):
    # "." and "" resolve to base exactly (the `full == base` branch).
    assert server._safe_path(".", workdir) == os.path.realpath(workdir)
    assert server._safe_path("", workdir) == os.path.realpath(workdir)


def test_safe_path_absolute_inside_base_honoured(workdir: str):
    # Absolute paths already inside base are honoured (open-terminal echoes cwd back).
    inside = os.path.realpath(os.path.join(workdir, "nested", "f.txt"))
    assert server._safe_path(inside, workdir) == inside


def test_safe_path_windows_separators_stay_confined(workdir: str):
    # Backslashes are NOT separators on Linux, so "..\\..\\etc\\passwd" is a single
    # literal filename component under base — it must NOT escape. The contract:
    # every escape returns 400 OR stays within base. Here it stays within base.
    res = server._safe_path("..\\..\\etc\\passwd", workdir)
    assert res == workdir or res.startswith(workdir + os.sep), f"backslash vector escaped: {res}"


# --- _request_base / X-Workspace-Subdir confinement (HTTP integration) --------

async def test_subdir_rejects_slashes(workdir: str, client):
    r = await client.get("/files/cwd", headers={"X-Workspace-Subdir": "a/b"})
    assert r.status_code == 400


async def test_subdir_rejects_traversal(workdir: str, client):
    # ".." passes the charset regex but _request_base's own escape check rejects it.
    r = await client.get("/files/cwd", headers={"X-Workspace-Subdir": ".."})
    assert r.status_code == 400


async def test_subdir_rejects_too_long(workdir: str, client):
    r = await client.get("/files/cwd", headers={"X-Workspace-Subdir": "x" * 65})
    assert r.status_code == 400


async def test_subdir_creates_and_confines(workdir: str, client):
    r = await client.get("/files/cwd", headers={"X-Workspace-Subdir": "chat1"})
    assert r.status_code == 200
    cwd = r.json()["cwd"]
    assert cwd.endswith("/chat1")
    assert cwd.startswith(workdir + os.sep)

    # write a secret in chat1
    r = await client.post(
        "/files/write",
        json={"path": "secret.txt", "content": "topsecret"},
        headers={"X-Workspace-Subdir": "chat1"},
    )
    assert r.status_code == 200

    # a *different* subdir cannot traverse into chat1
    r = await client.get(
        "/files/read",
        params={"path": "../chat1/secret.txt"},
        headers={"X-Workspace-Subdir": "chat2"},
    )
    assert r.status_code == 400
    # chat1 itself cannot traverse above WORKDIR either
    r = await client.get(
        "/files/read",
        params={"path": "../../etc/passwd"},
        headers={"X-Workspace-Subdir": "chat1"},
    )
    assert r.status_code == 400


# --- HTTP-level traversal against the endpoints -------------------------------

async def test_http_read_rejects_traversal(workdir: str, client):
    r = await client.get("/files/read", params={"path": "../../etc/passwd"})
    assert r.status_code == 400
    r = await client.get("/files/read", params={"path": "/etc/passwd"})
    assert r.status_code == 400
    r = await client.get("/files/read", params={"path": "%2e%2e/%2e%2e/etc/passwd"})
    assert r.status_code == 400


async def test_http_write_rejects_traversal(workdir: str, client):
    r = await client.post("/files/write", json={"path": "../../tmp/evil", "content": "x"})
    assert r.status_code == 400


async def test_http_list_rejects_traversal(workdir: str, client):
    r = await client.get("/files/list", params={"directory": "../../etc"})
    assert r.status_code == 400


async def test_http_delete_rejects_traversal(workdir: str, client):
    r = await client.delete("/files/delete", params={"path": "../../etc/passwd"})
    assert r.status_code == 400
