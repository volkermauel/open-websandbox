"""code-standard runtime server.

Hardened FastAPI app that mirrors the OWUI open-terminal surface and the
agent-sandbox python-runtime reference: POST /execute (stdout/stderr/exit_code)
plus file ops (/upload, /download, /list, /exists) and a GET / health check.

Hardening over the reference:
  * /execute runs in a NEW process group (start_new_session=True) so a timeout
    kills the WHOLE process tree (children), not just the shell.
  * per-request command timeout (default 120s, capped at MAX_TIMEOUT).
  * stdout/stderr truncated to MAX_OUTPUT_BYTES per stream.
  * all file ops are confined to WORKDIR (/workspace) via path normalization.

Runs as non-root uid 1000; WORKDIR=/workspace (an emptyDir at runtime).
"""
import asyncio
import contextlib
import datetime as _dt
import fcntl
import hmac
import json
import logging
import os
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import urllib.parse
import uuid as _uuid
from typing import Optional

from fastapi import FastAPI, File, Header, HTTPException, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse
from pydantic import BaseModel, Field

def _env_int(name: str, default: int) -> int:
    """Parse an int env var, falling back to `default` on missing/bad input."""
    try:
        return int(os.environ.get(name, str(default)))
    except (TypeError, ValueError):
        return default


WORKDIR = os.environ.get("WORKDIR", "/workspace")
MAX_OUT = _env_int("MAX_OUTPUT_BYTES", 1 << 20)  # 1 MiB / stream
DEFAULT_TIMEOUT = _env_int("DEFAULT_TIMEOUT", 120)
MAX_TIMEOUT = _env_int("MAX_TIMEOUT", 600)

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("code-standard")

app = FastAPI(
    title="code-standard runtime",
    description="Exec + file API for an agent sandbox (OWUI open-terminal surface).",
)


class ExecuteRequest(BaseModel):
    command: str
    timeout: Optional[int] = Field(default=None, ge=1, le=MAX_TIMEOUT)


class ExecuteResponse(BaseModel):
    stdout: str
    stderr: str
    exit_code: int
    timed_out: bool = False


def _cap(s: str) -> str:
    if len(s) > MAX_OUT:
        return s[:MAX_OUT] + f"\n...[truncated: {len(s) - MAX_OUT} more bytes]\n"
    return s


_SUBDIR_RE = re.compile(r"^[A-Za-z0-9._-]{1,64}$")


def _safe_path(rel: str, base: str = WORKDIR) -> str:
    """Resolve `rel` under `base` (default WORKDIR), rejecting escapes.

    When a per-chat `X-Workspace-Subdir` is in effect, `base` is WORKDIR/<subdir>;
    the same confinement applies.
    """
    rel = urllib.parse.unquote(rel).lstrip("/")
    base = os.path.realpath(base)
    full = os.path.realpath(os.path.join(base, rel))
    if full != base and not full.startswith(base + os.sep):
        raise HTTPException(status_code=400, detail="path escapes workspace")
    return full


def _request_base(subdir: Optional[str]) -> str:
    """Effective workspace base for this request: WORKDIR, or WORKDIR/<subdir>.

    The broker sets X-Workspace-Subdir on persistent-profile requests so each chat
    runs isolated under its own folder on the shared per-user PVC. The subdir is
    validated (no slashes / traversal) and created on first use.
    """
    if not subdir:
        return WORKDIR
    if not _SUBDIR_RE.match(subdir):
        raise HTTPException(status_code=400, detail="invalid X-Workspace-Subdir")
    base = os.path.realpath(os.path.join(WORKDIR, subdir))
    if base != WORKDIR and not base.startswith(WORKDIR + os.sep):
        raise HTTPException(status_code=400, detail="subdir escapes workspace")
    try:
        os.makedirs(base, exist_ok=True)
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"cannot create workspace subdir: {e}") from e
    return base


@app.get("/")
async def health():
    return {"status": "ok", "runtime": "code-standard"}


@app.post("/execute", response_model=ExecuteResponse)
async def execute(req: ExecuteRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    timeout = min(req.timeout or DEFAULT_TIMEOUT, MAX_TIMEOUT)
    timed_out = False
    log.info("exec (timeout=%ss): %s", timeout, req.command[:200])
    try:
        # Intentional shell execution: this endpoint IS the sandbox command tool
        # surface (OWUI open-terminal sends a shell string). Isolation is enforced by
        # the deployment boundary (gVisor runtimeClass, uid 1000, no service-account
        # token, restricted NetworkPolicy) — not by argument parsing here.
        proc = subprocess.Popen(  # noqa: S603,S602 - trusted tool surface
            req.command,
            shell=True,
            cwd=base,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,  # own process group -> tree-kill on timeout
        )
        try:
            stdout, stderr = proc.communicate(timeout=timeout)
            exit_code = proc.returncode
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_group(proc.pid)
            # SIGKILL'd process group is dead; communicate() drains the pipes and
            # reaps the process with no timeout (no further TimeoutExpired possible).
            stdout, stderr = proc.communicate()
            exit_code = 124
        return ExecuteResponse(stdout=_cap(stdout), stderr=_cap(stderr), exit_code=exit_code, timed_out=timed_out)
    except OSError as e:
        log.exception("exec failed")
        return ExecuteResponse(stdout="", stderr=f"runtime error: {e}", exit_code=1)


def _kill_group(pid: int) -> None:
    with contextlib.suppress(ProcessLookupError):
        os.killpg(os.getpgid(pid), signal.SIGKILL)


@app.post("/upload")
async def upload_file(file: UploadFile = File(...), subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    full = _safe_path(file.filename or "", base)
    try:
        os.makedirs(os.path.dirname(full) or base, exist_ok=True)
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"write failed: {e}") from e
    return {"saved": os.path.relpath(full, base), "bytes": os.path.getsize(full)}


@app.get("/download/{file_path:path}")
async def download_file(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="not found")
    return FileResponse(full, filename=os.path.basename(full))


@app.get("/list/{file_path:path}")
async def list_files(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    full = _safe_path(file_path, base)
    if not os.path.isdir(full):
        raise HTTPException(status_code=404, detail="not a directory")
    entries = []
    try:
        names = sorted(os.listdir(full))
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"list failed: {e}") from e
    for name in names:
        p = os.path.join(full, name)
        entries.append(
            {"name": name, "type": "dir" if os.path.isdir(p) else "file", "size": os.path.getsize(p)}
        )
    return {"path": os.path.relpath(full, base), "entries": entries}


@app.get("/exists/{file_path:path}")
async def exists(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path, _request_base(subdir))
    return {"exists": os.path.exists(full), "is_file": os.path.isfile(full), "is_dir": os.path.isdir(full)}


# --- interactive terminal (PTY) -------------------------------------------------
# open-terminal-compatible /api/terminals surface so OWUI's terminal UI connects
# unchanged. POST forks a shell on a PTY scoped to the chat workspace folder; the WS
# at /api/terminals/{id} streams BINARY stdin/stdout with TEXT
# {"type":"resize","cols":N,"rows":N} control frames, and honours a first-message
# {"type":"auth","token":...} handshake only when RUNTIME_API_KEY is set (default
# off: the broker already authenticated the caller and the runtime is not directly
# exposed). Verified under gVisor runsc: openpty/fork/ioctl TIOCSWINSZ/select are all
# emulated. One terminal per chat (session id = X-Session-Id); destroyed on WS close.

MAX_TERMINAL_SESSIONS = _env_int("MAX_TERMINAL_SESSIONS", 8)
_SHELL = os.environ.get("SHELL", "/bin/bash")
_terminals: dict[str, dict] = {}


def _term_alive(s: dict) -> bool:
    return s["proc"].poll() is None


def _term_cleanup(sid: str) -> None:
    s = _terminals.pop(sid, None)
    if not s:
        return
    with contextlib.suppress(ProcessLookupError):
        os.killpg(os.getpgid(s["proc"].pid), signal.SIGKILL)
    with contextlib.suppress(OSError):
        os.close(s["master_fd"])


def _term_write(master_fd: int, data: bytes) -> None:
    with contextlib.suppress(OSError):
        os.write(master_fd, data)


@app.post("/api/terminals")
async def create_terminal(
    subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir"),
    session_id: Optional[str] = Header(default=None, alias="X-Session-Id"),
):
    base = _request_base(subdir)
    for sid in [s for s, v in _terminals.items() if not _term_alive(v)]:
        _term_cleanup(sid)
    if len(_terminals) >= MAX_TERMINAL_SESSIONS:
        raise HTTPException(status_code=429, detail=f"max {MAX_TERMINAL_SESSIONS} terminals reached")
    sid = session_id or str(_uuid.uuid4())[:8]
    if sid in _terminals:
        _term_cleanup(sid)
    master_fd = slave_fd = -1
    try:
        master_fd, slave_fd = pty.openpty()
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        proc = subprocess.Popen(  # noqa: S603 - spawns the interactive shell (trusted tool surface)
            [_SHELL], stdin=slave_fd, stdout=slave_fd, stderr=slave_fd, cwd=base,
            start_new_session=True, env={**os.environ, "TERM": "xterm-256color"},
        )
    except OSError as e:
        for fd in (slave_fd, master_fd):
            if fd >= 0:
                with contextlib.suppress(OSError):
                    os.close(fd)
        raise HTTPException(status_code=503, detail=f"pty spawn failed: {e}") from e
    os.close(slave_fd)
    fcntl.fcntl(master_fd, fcntl.F_SETFL, os.O_NONBLOCK)
    created = _dt.datetime.now(_dt.timezone.utc).isoformat().replace("+00:00", "Z")
    _terminals[sid] = {"master_fd": master_fd, "proc": proc, "created_at": created}
    log.info("terminal %s created (pid=%s cwd=%s)", sid, proc.pid, base)
    return {"id": sid, "created_at": created, "pid": proc.pid}


@app.get("/api/terminals")
async def list_terminals():
    out, dead = [], []
    for sid, s in _terminals.items():
        if _term_alive(s):
            out.append({"id": sid, "created_at": s["created_at"], "pid": s["proc"].pid})
        else:
            dead.append(sid)
    for sid in dead:
        _term_cleanup(sid)
    return out


@app.get("/api/terminals/{session_id}")
async def get_terminal(session_id: str):
    s = _terminals.get(session_id)
    if not s or not _term_alive(s):
        if session_id in _terminals:
            _term_cleanup(session_id)
        raise HTTPException(status_code=404, detail="terminal not found")
    return {"id": session_id, "created_at": s["created_at"], "pid": s["proc"].pid}


@app.delete("/api/terminals/{session_id}")
async def kill_terminal(session_id: str):
    _term_cleanup(session_id)
    return {"status": "deleted"}


@app.websocket("/api/terminals/{session_id}")
async def terminal_ws(ws: WebSocket, session_id: str):
    await ws.accept()
    s = _terminals.get(session_id)
    if not s or not _term_alive(s):
        if session_id in _terminals:
            _term_cleanup(session_id)
        await ws.close(code=4004, reason="unknown or ended session")
        return
    rtkey = os.environ.get("RUNTIME_API_KEY", "")
    if rtkey:
        try:
            payload = json.loads(await asyncio.wait_for(ws.receive_text(), timeout=10.0))
            if payload.get("type") != "auth" or not hmac.compare_digest(str(payload.get("token", "")), rtkey):
                await ws.close(code=4001, reason="invalid api key")
                return
        except Exception:
            await ws.close(code=4001, reason="auth timeout or invalid payload")
            return

    master_fd, proc, stop = s["master_fd"], s["proc"], asyncio.Event()
    loop = asyncio.get_event_loop()

    def _blocking_read() -> Optional[bytes]:
        while not stop.is_set():
            ready, _, _ = select.select([master_fd], [], [], 0.1)
            if ready:
                try:
                    return os.read(master_fd, 4096)
                except OSError:
                    return b""
            if proc.poll() is not None:
                return b""
        return None

    async def _pty_reader():
        try:
            while not stop.is_set():
                data = await loop.run_in_executor(None, _blocking_read)
                if data is None:
                    break
                if not data:
                    if proc.poll() is not None:
                        break
                    continue
                try:
                    await ws.send_bytes(data)
                except Exception:
                    break
        except Exception:
            pass

    reader = asyncio.create_task(_pty_reader())
    try:
        while True:
            msg = await ws.receive()
            if msg["type"] == "websocket.disconnect":
                break
            if msg.get("bytes"):
                await loop.run_in_executor(None, _term_write, master_fd, msg["bytes"])
            elif msg.get("text"):
                try:
                    payload = json.loads(msg["text"])
                    if payload.get("type") == "resize":
                        rows, cols = int(payload.get("rows", 24)), int(payload.get("cols", 80))
                        with contextlib.suppress(OSError):
                            fcntl.ioctl(master_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                except (json.JSONDecodeError, ValueError, TypeError):
                    pass
    except WebSocketDisconnect:
        pass
    finally:
        stop.set()
        reader.cancel()
        with contextlib.suppress(Exception):
            await reader
        _term_cleanup(session_id)
