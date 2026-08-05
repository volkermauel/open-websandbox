"""Extra coverage for the interactive-terminal (PTY) surface.

Split into:
  * HTTP-only edge cases (``client`` fixture, in-process ASGI): dead-session
    reaping on create/list/get, the MAX_TERMINAL_SESSIONS 429, the existing-id
    recreate, and the pty-spawn-failure 503 (a deliberately-bad SHELL path —
    the real ``pty.openpty`` succeeds, the real ``Popen`` raises).
  * Direct unit test of ``server._term_write``: the OSError-swallow break.
  * WebSocket edge cases (``live_base`` fixture, real uvicorn + real PTY): the
    auth handshake (valid / wrong-token / bad-json), a malformed control frame,
    a shell ``exit`` (PTY EIO → EOF sentinel), a background job holding the
    slave so the heartbeat proc-death path fires, and an idle heartbeat.

Every PTY here is REAL — no mocking of openpty/fork/ioctl. We only kill real
process groups to manufacture "dead but still listed" sessions.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import os
import signal

import httpx
import pytest
import websockets

import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert


# --- helpers ------------------------------------------------------------------

def _kill_terminal_proc(sid: str) -> None:
    """Kill a terminal's shell WITHOUT removing its dict entry (→ 'dead' entry).

    The endpoints' dead-session reaping only fires for entries that are present
    but whose proc has exited, so we must bypass ``_term_cleanup`` (which pops).
    """
    s = server._terminals[sid]
    with contextlib.suppress(ProcessLookupError):
        os.killpg(os.getpgid(s["proc"].pid), signal.SIGKILL)
    s["proc"].wait()


async def _recv_until(ws, marker: bytes, timeout: float = 5.0) -> bytes:
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


async def _wait_cleaned(http: httpx.AsyncClient, sid: str, timeout: float = 5.0) -> bool:
    """Poll GET /api/terminals/{sid} until it 404s (async disconnect cleanup)."""
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if (await http.get(f"/api/terminals/{sid}")).status_code == 404:
            return True
        await asyncio.sleep(0.1)
    return False


# --- HTTP-only terminal edge cases -------------------------------------------

async def test_create_terminal_reaps_dead_sessions(workdir, client):
    # A dead-but-listed session must be reaped when a new terminal is created.
    await client.post("/api/terminals", headers={"X-Session-Id": "dead1"})
    _kill_terminal_proc("dead1")
    assert "dead1" in server._terminals
    assert not server._term_alive(server._terminals["dead1"])

    r = await client.post("/api/terminals", headers={"X-Session-Id": "fresh1"})
    assert r.status_code == 200
    assert r.json()["id"] == "fresh1"
    # the dead session was cleaned up by the create path
    assert "dead1" not in server._terminals
    server._term_cleanup("fresh1")


async def test_create_terminal_429_when_max_reached(workdir, client, monkeypatch):
    monkeypatch.setattr(server, "MAX_TERMINAL_SESSIONS", 1)
    r = await client.post("/api/terminals", headers={"X-Session-Id": "max1"})
    assert r.status_code == 200
    r = await client.post("/api/terminals", headers={"X-Session-Id": "max2"})
    assert r.status_code == 429
    assert "max" in r.json()["detail"].lower()
    server._term_cleanup("max1")


async def test_create_terminal_recreates_existing_id(workdir, client):
    r1 = await client.post("/api/terminals", headers={"X-Session-Id": "dup"})
    assert r1.status_code == 200
    pid1 = r1.json()["pid"]
    # same X-Session-Id again → old session torn down, a brand-new one spawned
    r2 = await client.post("/api/terminals", headers={"X-Session-Id": "dup"})
    assert r2.status_code == 200
    pid2 = r2.json()["pid"]
    assert pid2 != pid1
    assert server._terminals["dup"]["proc"].pid == pid2
    server._term_cleanup("dup")


async def test_create_terminal_spawn_failure_is_503(workdir, client, monkeypatch):
    # A non-existent SHELL: the real pty.openpty succeeds, the real Popen raises
    # FileNotFoundError (OSError) → 503. No pty mocking — a genuine spawn fail.
    monkeypatch.setattr(server, "_SHELL", "/nonexistent/shell-xyz-12345")
    r = await client.post("/api/terminals", headers={"X-Session-Id": "badspawn"})
    assert r.status_code == 503
    assert "pty spawn failed" in r.json()["detail"]
    assert "badspawn" not in server._terminals  # entry never inserted on failure


async def test_list_terminals_reaps_dead(workdir, client):
    await client.post("/api/terminals", headers={"X-Session-Id": "ldead"})
    _kill_terminal_proc("ldead")
    r = await client.get("/api/terminals")
    assert r.status_code == 200
    assert r.json() == []  # the dead session was reaped during the listing
    assert "ldead" not in server._terminals


async def test_list_terminals_reports_live(workdir, client):
    await client.post("/api/terminals", headers={"X-Session-Id": "live1"})
    r = await client.get("/api/terminals")
    ids = {t["id"] for t in r.json()}
    assert "live1" in ids
    server._term_cleanup("live1")


async def test_get_terminal_dead_session_cleaned_and_404(workdir, client):
    await client.post("/api/terminals", headers={"X-Session-Id": "gdead"})
    _kill_terminal_proc("gdead")
    r = await client.get("/api/terminals/gdead")
    assert r.status_code == 404
    assert "gdead" not in server._terminals  # cleaned on read


async def test_delete_terminal_idempotent_for_missing(workdir, client):
    # DELETE of a session that was never created still reports deleted.
    r = await client.delete("/api/terminals/never-existed")
    assert r.status_code == 200
    assert r.json() == {"status": "deleted"}


# --- _term_write OSError break (unit) ----------------------------------------

def test_term_write_oserror_is_swallowed():
    # os.write on a bad fd raises OSError → _term_write must catch it and break
    # the loop WITHOUT propagating (it runs on the event-loop thread).
    # Must not raise:
    server._term_write(9_876_543, b"x" * 4096)


# --- WebSocket edge cases ----------------------------------------------------

async def test_terminal_ws_dead_session_is_cleaned(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "wsdead"})
        _kill_terminal_proc("wsdead")  # entry present but dead
        async with websockets.connect(f"{ws_base}/api/terminals/wsdead") as ws:
            with pytest.raises(websockets.ConnectionClosed) as ei:
                await ws.recv()
            assert ei.value.code == 4004
        # the dead entry was cleaned by the WS handler before closing
        assert "wsdead" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("wsdead")


async def test_terminal_ws_malformed_control_frame_ignored(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "mal"})
        async with websockets.connect(f"{ws_base}/api/terminals/mal") as ws:
            # a non-JSON text frame → inner except → ignored, loop continues
            await ws.send("this is not json at all")
            await asyncio.sleep(0.2)
            # receiver must still be alive and draining input
            await ws.send(b"echo aftermal\n")
            echoed = await _recv_until(ws, b"aftermal")
            assert b"aftermal" in echoed
        assert await _wait_cleaned(http, "mal")
        assert "mal" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("mal")


async def test_terminal_ws_control_frames_then_bytes(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "ctrl"})
        async with websockets.connect(f"{ws_base}/api/terminals/ctrl") as ws:
            # resize (valid control) → handled, loop continues
            await ws.send(json.dumps({"type": "resize", "rows": 20, "cols": 90}))
            await asyncio.sleep(0.15)
            # then bytes still flow
            await ws.send(b"echo ctrlok\n")
            echoed = await _recv_until(ws, b"ctrlok")
            assert b"ctrlok" in echoed
        assert await _wait_cleaned(http, "ctrl")
        assert "ctrl" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("ctrl")


async def test_terminal_ws_exit_drains_and_cleans(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "ex"})
        async with websockets.connect(f"{ws_base}/api/terminals/ex") as ws:
            await ws.send(b"exit\n")
            # the shell exits → PTY EIO/EOF sentinel → reader ends → WS closes,
            # and the finally block tears the PTY down.
            for _ in range(40):
                try:
                    await asyncio.wait_for(ws.recv(), timeout=0.2)
                except (websockets.ConnectionClosed, asyncio.TimeoutError):
                    pass
                if (await http.get("/api/terminals/ex")).status_code == 404:
                    break
        assert "ex" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("ex")


async def test_terminal_ws_idle_heartbeat_keeps_alive(live_base):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "idle"})
        async with websockets.connect(f"{ws_base}/api/terminals/idle") as ws:
            # >1s of silence with a LIVE shell → heartbeat "continue" path fires.
            await asyncio.sleep(2.0)
            assert (await http.get("/api/terminals/idle")).status_code == 200
            # and the session is still fully functional afterwards
            await ws.send(b"echo idletext\n")
            echoed = await _recv_until(ws, b"idletext")
            assert b"idletext" in echoed
        assert await _wait_cleaned(http, "idle")
        assert "idle" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("idle")


async def test_terminal_ws_background_job_heartbeat_death(live_base):
    """The documented edge case: shell exits but a backgrounded job keeps the
    PTY slave open, so no EIO/EOF arrives on the master fd. The reader's 1s
    heartbeat must then notice the dead proc and tear the session down."""
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "bg"})
        async with websockets.connect(f"{ws_base}/api/terminals/bg") as ws:
            # background a long sleep (holds the slave open), then leave shell.
            await ws.send(b"sleep 120 &\n")
            await asyncio.sleep(0.4)
            await ws.send(b"exit\n")
            # poll for the heartbeat-driven teardown (≤ ~3s after last output)
            cleaned = False
            for _ in range(60):
                if (await http.get("/api/terminals/bg")).status_code == 404:
                    cleaned = True
                    break
                await asyncio.sleep(0.1)
                with contextlib.suppress(Exception):
                    await asyncio.wait_for(ws.recv(), timeout=0.05)
            assert cleaned, "heartbeat did not tear down the orphaned session"
        assert await _wait_cleaned(http, "bg")
        assert "bg" not in server._terminals
    finally:
        # _term_cleanup SIGKILLs the whole group → also reaps the orphan sleep
        server._term_cleanup("bg")
        await http.aclose()


# --- WebSocket auth handshake (RUNTIME_API_KEY set) --------------------------

async def test_terminal_ws_auth_success(live_base, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "authok"})
        async with websockets.connect(f"{ws_base}/api/terminals/authok") as ws:
            await ws.send(json.dumps({"type": "auth", "token": "s3cret-key"}))
            await asyncio.sleep(0.2)
            await ws.send(b"echo authedok\n")
            echoed = await _recv_until(ws, b"authedok")
            assert b"authedok" in echoed
        assert await _wait_cleaned(http, "authok")
        assert "authok" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("authok")


async def test_terminal_ws_auth_wrong_token(live_base, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "authbad"})
        async with websockets.connect(f"{ws_base}/api/terminals/authbad") as ws:
            await ws.send(json.dumps({"type": "auth", "token": "wrong"}))
            with pytest.raises(websockets.ConnectionClosed) as ei:
                await ws.recv()
            assert ei.value.code == 4001
    finally:
        await http.aclose()
        server._term_cleanup("authbad")


async def test_terminal_ws_auth_bad_payload(live_base, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "authjson"})
        async with websockets.connect(f"{ws_base}/api/terminals/authjson") as ws:
            # a non-JSON first frame → json.loads raises → except → close 4001
            await ws.send("<<<not json>>>")
            with pytest.raises(websockets.ConnectionClosed) as ei:
                await ws.recv()
            assert ei.value.code == 4001
    finally:
        await http.aclose()
        server._term_cleanup("authjson")
