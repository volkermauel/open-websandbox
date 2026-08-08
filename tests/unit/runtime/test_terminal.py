# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Interactive terminal (PTY) tests over the real WebSocket.

``POST /api/terminals/{id}`` forks a shell on a real ``pty.openpty``; the WS at
``/api/terminals/{id}`` streams BINARY stdin/stdout with TEXT resize control
frames. Because httpx's ASGI transport cannot upgrade to WebSocket, we stand up
a real uvicorn server (``live_base``) and drive it with the async ``websockets``
client.

Coverage: create terminal -> connect WS -> send ``echo hi`` and assert the bytes
come back -> send a resize control frame and prove the session stays alive ->
disconnect and assert the PTY is cleaned up (terminal entry gone).
"""

from __future__ import annotations

import asyncio
import json

import httpx
import pytest
import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert
import websockets

SESSION_ID = "wstest-1"


async def _recv_until(ws, marker: bytes, timeout: float = 4.0) -> bytes:
    """Accumulate received binary frames until `marker` appears or timeout."""
    buf = b""
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while marker not in buf:
        remaining = deadline - loop.time()
        if remaining <= 0:
            break
        try:
            msg = await asyncio.wait_for(ws.recv(), timeout=remaining)
        except asyncio.TimeoutError:
            break
        if isinstance(msg, (bytes, bytearray)):
            buf += bytes(msg)
    return buf


async def test_terminal_echo_resize_and_cleanup(workdir, live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)

    try:
        # 1. create the terminal in the workspace
        r = await http.post("/api/terminals", headers={"X-Session-Id": SESSION_ID})
        assert r.status_code == 200, r.text
        term = r.json()
        assert term["id"] == SESSION_ID
        assert term["pid"] > 0

        # it shows up in GET /api/terminals/{id}
        r = await http.get(f"/api/terminals/{SESSION_ID}")
        assert r.status_code == 200

        # 2. connect the WS and send `echo hi` -> bytes must come back over the PTY
        async with websockets.connect(f"{ws_base}/api/terminals/{SESSION_ID}") as ws:
            await ws.send(b"echo hi\n")
            echoed = await _recv_until(ws, b"hi")
            assert b"hi" in echoed, f"PTY did not echo `hi` back: {echoed!r}"

            # 3. send a TEXT resize control frame; it must not error or close the WS.
            await ws.send(json.dumps({"type": "resize", "rows": 30, "cols": 100}))

            # Prove the session is still alive by running another command afterwards.
            await ws.send(b"echo afterresize\n")
            echoed = await _recv_until(ws, b"afterresize")
            assert b"afterresize" in echoed, "session died after resize frame"

        # 4. disconnect -> the server must clean up the PTY (terminal entry gone).
        #    Cleanup happens async on the server loop, so poll briefly.
        cleaned = False
        for _ in range(40):
            r = await http.get(f"/api/terminals/{SESSION_ID}")
            if r.status_code == 404:
                cleaned = True
                break
            await asyncio.sleep(0.1)
        assert cleaned, "terminal entry survived WS disconnect (PTY leak)"
        # same process -> the module dict should have dropped it too
        assert SESSION_ID not in server._terminals
    finally:
        await http_aclose(http)
        # defensive: make sure no PTY leaks even if an assertion failed mid-test
        server._term_cleanup(SESSION_ID)


async def test_terminal_unknown_session_rejected(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        # WS to a session id that was never created -> server closes with 4004.
        async with websockets.connect(f"{ws_base}/api/terminals/never-made") as ws:
            # The server accepts then immediately closes with code 4004.
            with pytest.raises(websockets.ConnectionClosed):
                await ws.recv()
    finally:
        await http_aclose(http)


async def test_terminal_create_and_delete_via_http(live_base, rt_auth):
    _, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
    try:
        r = await http.post("/api/terminals", headers={"X-Session-Id": "http-only"})
        assert r.status_code == 200
        assert r.json()["id"] == "http-only"
        # explicit DELETE tears it down
        r = await http.delete("/api/terminals/http-only")
        assert r.status_code == 200
        assert r.json() == {"status": "deleted"}
        # now gone
        assert (await http.get("/api/terminals/http-only")).status_code == 404
    finally:
        await http_aclose(http)
        server._term_cleanup("http-only")


async def http_aclose(http: httpx.AsyncClient) -> None:
    await http.aclose()
