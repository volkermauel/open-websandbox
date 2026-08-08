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
import pty
import signal
import subprocess

import httpx
import pytest
import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert
import websockets

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


def _rt_headers() -> dict:
    """Inter-component Bearer for cleanup/readiness polls that must clear _auth_runtime.

    Read from os.environ at CALL time so the WS auth tests (which monkeypatch
    RUNTIME_API_KEY='s3cret-key') poll with the live key, not the conftest default.
    """
    return {"Authorization": f"Bearer {os.environ.get('RUNTIME_API_KEY', '')}"}


async def _wait_cleaned(http: httpx.AsyncClient, sid: str, timeout: float = 5.0) -> bool:
    """Poll GET /api/terminals/{sid} until it 404s (async disconnect cleanup)."""
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if (await http.get(f"/api/terminals/{sid}", headers=_rt_headers())).status_code == 404:
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

async def test_terminal_ws_dead_session_is_cleaned(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
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


async def test_terminal_ws_malformed_control_frame_ignored(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
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


async def test_terminal_ws_control_frames_then_bytes(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
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


async def test_terminal_ws_exit_drains_and_cleans(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
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
                if (await http.get("/api/terminals/ex", headers=_rt_headers())).status_code == 404:
                    break
        assert "ex" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("ex")


async def test_terminal_ws_idle_heartbeat_keeps_alive(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "idle"})
        async with websockets.connect(f"{ws_base}/api/terminals/idle") as ws:
            # >1s of silence with a LIVE shell → heartbeat "continue" path fires.
            await asyncio.sleep(2.0)
            assert (await http.get("/api/terminals/idle", headers=_rt_headers())).status_code == 200
            # and the session is still fully functional afterwards
            await ws.send(b"echo idletext\n")
            echoed = await _recv_until(ws, b"idletext")
            assert b"idletext" in echoed
        assert await _wait_cleaned(http, "idle")
        assert "idle" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("idle")


async def test_terminal_ws_background_job_heartbeat_death(live_base, rt_auth):
    """The documented edge case: shell exits but a backgrounded job keeps the
    PTY slave open, so no EIO/EOF arrives on the master fd. The reader's 1s
    heartbeat must then notice the dead proc and tear the session down."""
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
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
                if (await http.get("/api/terminals/bg", headers=_rt_headers())).status_code == 404:
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


# --- WebSocket inter-component auth (RUNTIME_API_KEY set) -------------------

async def test_terminal_ws_auth_success(live_base, monkeypatch):
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "authok",
                                                   "Authorization": "Bearer s3cret-key"})
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
        await http.post("/api/terminals", headers={"X-Session-Id": "authbad",
                                                   "Authorization": "Bearer s3cret-key"})
        async with websockets.connect(f"{ws_base}/api/terminals/authbad") as ws:
            await ws.send(json.dumps({"type": "auth", "token": "wrong"}))
            with pytest.raises(websockets.ConnectionClosed) as ei:
                await ws.recv()
            assert ei.value.code == 4001
    finally:
        await http.aclose()
        server._term_cleanup("authbad")


async def test_terminal_ws_non_auth_first_frame_tolerated(live_base, monkeypatch):
    # Broker-compat: with RUNTIME_API_KEY set, a non-auth FIRST frame must NOT close the
    # session. The broker consumes OWUI's auth frame upstream and forwards raw bytes, so
    # the runtime may receive terminal input (or a malformed control frame) before any
    # auth frame. Auth is enforced only for an actual {"type":"auth",...} frame (see
    # test_terminal_ws_auth_wrong_token); a non-JSON frame is ignored, input still flows.
    monkeypatch.setenv("RUNTIME_API_KEY", "s3cret-key")
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "authjson",
                                                   "Authorization": "Bearer s3cret-key"})
        async with websockets.connect(f"{ws_base}/api/terminals/authjson") as ws:
            # a non-JSON text frame -> ignored (malformed control), session stays alive
            await ws.send("<<<not json>>>")
            await asyncio.sleep(0.2)
            # terminal input still flows afterwards
            await ws.send(b"echo afterbad\n")
            echoed = await _recv_until(ws, b"afterbad")
            assert b"afterbad" in echoed
        assert await _wait_cleaned(http, "authjson")
        assert "authjson" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("authjson")


# --- create_terminal 503 paths -------------------------------------------------

async def test_create_terminal_openpty_failure_503(workdir, client, monkeypatch):
    # openpty() raises before any fd is assigned -> slave_fd/master_fd stay at
    # their -1 initial -> the `if fd >= 0:` cleanup branch is False (skipped).
    def _openpty_boom():
        raise OSError("openpty unavailable")

    monkeypatch.setattr(pty, "openpty", _openpty_boom)
    r = await client.post("/api/terminals", headers={"X-Session-Id": "nopenpty"})
    assert r.status_code == 503
    assert "pty spawn failed" in r.json()["detail"]
    # spawn failed before _terminals[sid] was populated
    assert "nopenpty" not in server._terminals


async def test_create_terminal_popen_failure_closes_fds_503(workdir, client, monkeypatch):
    # Real openpty() succeeds (valid fds assigned) but Popen raises -> the
    # `if fd >= 0:` branch is True and both fds are os.close()'d in cleanup.
    def _popen_boom(*args, **kwargs):
        raise OSError("popen boom")

    monkeypatch.setattr(subprocess, "Popen", _popen_boom)
    r = await client.post("/api/terminals", headers={"X-Session-Id": "nopopen"})
    assert r.status_code == 503
    assert "pty spawn failed" in r.json()["detail"]
    assert "nopopen" not in server._terminals


# --- _receiver inbound message-type branches (lines 799-812) --------------------

async def test_terminal_ws_receiver_message_branches(live_base, rt_auth):
    # Drive every inbound branch of the per-terminal _receiver loop: a bytes
    # frame (if-bytes True), a resize control (elif-text True + type==resize
    # True), a non-resize control (type!=resize -> inner-if False) and an empty
    # text frame (bytes falsy AND text "" falsy -> elif False).
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "rmsg"})
        async with websockets.connect(f"{ws_base}/api/terminals/rmsg") as ws:
            # (a) bytes frame -> if msg.get("bytes") True -> _term_write
            await ws.send(b"echo branch_a_ok\n")
            assert b"branch_a_ok" in await _recv_until(ws, b"branch_a_ok", timeout=5.0)

            # (b) resize control (text frame, type=resize) -> elif True + inner True
            await ws.send(json.dumps({"type": "resize", "cols": 40, "rows": 12}))
            await asyncio.sleep(0.25)

            # (c) non-resize text control -> payload type != resize -> inner False
            await ws.send(json.dumps({"type": "something_else"}))
            await asyncio.sleep(0.25)

            # (d) empty text frame -> bytes is None (falsy) reach elif, text "" falsy
            await ws.send("")
            await asyncio.sleep(0.25)

            # the terminal must still be usable after all control frames
            await ws.send(b"echo stillalive\n")
            assert b"stillalive" in await _recv_until(ws, b"stillalive", timeout=5.0)

        # client disconnect -> server _receiver ends and the session is reaped
        assert await _wait_cleaned(http, "rmsg", timeout=8.0)
        assert "rmsg" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("rmsg")


# --- heartbeat death detection (line 782) ---------------------------------------
# The existing background-job test uses plain `sleep 120 &`; when the session
# leader (bash) exits the terminal layer drops the slave, so the master read
# raises EIO (line 763) and the reader breaks BEFORE the 1s heartbeat can fire
# -> line 782 stays uncovered. A SIGHUP/TERM-immune grandchild keeps holding
# the slave open (master read just returns EAGAIN forever), so the ONLY death
# signal is the reader's per-second `proc.poll()` heartbeat -> break@782.

async def test_terminal_ws_heartbeat_death_with_hup_immune_job(live_base, rt_auth):
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "hb"})
        async with websockets.connect(f"{ws_base}/api/terminals/hb") as ws:
            # survivor ignores HUP/TERM -> outlives the shell, keeps slave open
            await ws.send(b"( trap '' HUP TERM; sleep 300 ) &\n")
            await asyncio.sleep(0.4)
            await ws.send(b"exit\n")
            # drain shell output until the server closes the WS (heartbeat fired)
            with contextlib.suppress(websockets.ConnectionClosed, asyncio.TimeoutError):
                while True:
                    await asyncio.wait_for(ws.recv(), timeout=0.2)
        assert await _wait_cleaned(http, "hb", timeout=8.0)
        assert "hb" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("hb")


# --- _pty_reader send failure (lines 788-789) -----------------------------------

async def test_terminal_ws_reader_send_failure(live_base, rt_auth):
    # Flood PTY output so the reader is constantly relaying, then abruptly drop
    # the client TCP transport. With the client gone the reader's next
    # `await ws.send_bytes(data)` raises on the dead connection -> except -> break.
    ws_base, http_base = live_base
    http = httpx.AsyncClient(base_url=http_base, headers=rt_auth)
    try:
        await http.post("/api/terminals", headers={"X-Session-Id": "sfail"})
        ws = await websockets.connect(f"{ws_base}/api/terminals/sfail")
        try:
            await ws.send(b"yes flooooood\n")
            # confirm the relay is flowing before we cut the cord
            with contextlib.suppress(asyncio.TimeoutError, websockets.ConnectionClosed):
                await asyncio.wait_for(ws.recv(), timeout=2.0)
            # abruptly drop the underlying TCP socket (no WS close handshake)
            transport = getattr(ws, "transport", None)
            if transport is not None:
                with contextlib.suppress(Exception):
                    transport.close()
            await asyncio.sleep(1.0)
        finally:
            with contextlib.suppress(Exception):
                await ws.close()
        assert await _wait_cleaned(http, "sfail", timeout=10.0)
        assert "sfail" not in server._terminals
    finally:
        await http.aclose()
        server._term_cleanup("sfail")
