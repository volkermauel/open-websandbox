# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""S3-tiered snapshot/restore endpoint tests (issue #52).

The broker is the sole S3 client; the runtime only streams a zstd-compressed tar of the
whole workspace off (GET /snapshot) and back on (PUT /restore) over the per-session key.
These tests drive the round-trip on a real tmp workspace (the `tar`+`zstd` CLIs are
present) + the size fail-on-exceed (D9) + auth gating.
"""
from __future__ import annotations

from pathlib import Path

import httpx
import server  # type: ignore[import-not-found]


# --- round-trip -------------------------------------------------------------------
async def test_snapshot_restore_roundtrip(client: httpx.AsyncClient, workdir: str, tmp_path: Path):
    # populate the workspace
    (Path(workdir) / "hello.txt").write_text("hello world")
    nested = Path(workdir) / "sub" / "deep"
    nested.mkdir(parents=True)
    (nested / "data.bin").write_bytes(b"\x00\x01\x02" * 100)

    r = await client.get("/snapshot")
    assert r.status_code == 200, r.text
    assert r.headers["content-type"] == "application/zstd"
    blob = r.content
    assert blob[:4] == b"\x28\xb5\x2f\xfd"  # zstd magic

    # restore into a FRESH empty workspace
    fresh = tmp_path / "restored"
    fresh.mkdir()
    server.WORKDIR = str(fresh)
    try:
        r2 = await client.put("/restore", content=blob)
        assert r2.status_code == 200, r2.text
        assert (fresh / "hello.txt").read_text() == "hello world"
        assert (fresh / "sub" / "deep" / "data.bin").read_bytes() == b"\x00\x01\x02" * 100
    finally:
        server.WORKDIR = workdir  # restore for fixture teardown safety


# --- size fail-on-exceed (D9) -----------------------------------------------------
async def test_snapshot_refuses_oversized(client: httpx.AsyncClient, workdir: str, monkeypatch):
    monkeypatch.setattr(server, "SNAPSHOT_MAX_BYTES", 64)
    (Path(workdir) / "big.bin").write_bytes(b"x" * 4096)
    r = await client.get("/snapshot")
    assert r.status_code == 413


async def test_restore_refuses_oversized(client: httpx.AsyncClient, workdir: str, monkeypatch):
    monkeypatch.setattr(server, "SNAPSHOT_MAX_BYTES", 64)
    r = await client.put("/restore", content=b"x" * 4096)
    assert r.status_code == 413


# --- auth gating (per-session key, #50) ------------------------------------------
async def test_snapshot_requires_auth(client_noauth: httpx.AsyncClient):
    r = await client_noauth.get("/snapshot")
    assert r.status_code in (401, 403)


async def test_restore_requires_auth(client_noauth: httpx.AsyncClient):
    r = await client_noauth.put("/restore", content=b"")
    assert r.status_code in (401, 403)
