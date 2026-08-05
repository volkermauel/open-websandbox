"""code-standard runtime server.

Hardened FastAPI app that mirrors the OWUI open-terminal surface and the
agent-sandbox python-runtime reference: POST /execute (stdout/stderr/exit_code)
plus the open-terminal /files/* + /ports surface and a GET / health check.

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
import fnmatch
import hmac
import io
import json
import logging
import mimetypes
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import termios
import urllib.parse
import uuid as _uuid
import zipfile
from typing import Optional

from fastapi import FastAPI, File, Header, HTTPException, Query, UploadFile, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, Response
from pydantic import BaseModel, Field

def _env_int(name: str, default: int) -> int:
    """Parse an int env var, falling back to `default` on missing/bad input."""
    try:
        return int(os.environ.get(name, str(default)))
    except (TypeError, ValueError):
        return default

# Bound per-sandbox process count (RLIMIT_NPROC) — caps fork bombs. gVisor enforces
# it (proven). Set at import time so uvicorn + all exec'd subprocesses inherit it.
try:
    import resource as _rlimit_mod
    _nproc = _env_int("MAX_PROCS", 256)
    _rlimit_mod.setrlimit(_rlimit_mod.RLIMIT_NPROC, (_nproc, _nproc))
except (ValueError, OSError, AttributeError):
    pass

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

    Relative paths are joined to `base`; absolute paths already inside `base` are
    honoured as-is (the open-terminal UI echoes the absolute cwd back from GET
    /files/cwd). Any path that resolves outside `base` is rejected.

    When a per-chat `X-Workspace-Subdir` is in effect, `base` is WORKDIR/<subdir>;
    the same confinement applies.
    """
    rel = urllib.parse.unquote(rel or "")
    base = os.path.realpath(base)
    if os.path.isabs(rel):
        full = os.path.realpath(rel)
    else:
        full = os.path.realpath(os.path.join(base, rel.lstrip("/")))
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
        #
        # asyncio subprocess keeps this NON-BLOCKING: the event loop multiplexes the
        # wait, so a long command neither pins a worker thread nor freezes the rest of
        # the runtime (file ops, interactive terminals). Verified concurrent under gVisor.
        proc = await asyncio.create_subprocess_shell(  # noqa: S603,S602
            req.command,
            cwd=base,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            start_new_session=True,  # own process group -> tree-kill on timeout
        )
        try:
            out_b, err_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
            exit_code = proc.returncode if proc.returncode is not None else 0
        except asyncio.TimeoutError:
            timed_out = True
            _kill_group(proc.pid)
            await proc.wait()  # reap the SIGKILL'd process group
            out_b, err_b = b"", b""
            exit_code = 124
        stdout = out_b.decode(errors="replace") if isinstance(out_b, (bytes, bytearray)) else (out_b or "")
        stderr = err_b.decode(errors="replace") if isinstance(err_b, (bytes, bytearray)) else (err_b or "")
        return ExecuteResponse(stdout=_cap(stdout), stderr=_cap(stderr), exit_code=exit_code, timed_out=timed_out)
    except OSError as e:
        log.exception("exec failed")
        return ExecuteResponse(stdout="", stderr=f"runtime error: {e}", exit_code=1)


def _kill_group(pid: int) -> None:
    with contextlib.suppress(ProcessLookupError):
        os.killpg(os.getpgid(pid), signal.SIGKILL)


# --- open-terminal filesystem surface ---------------------------------------
# Mirrors ghcr.io/open-webui/open-terminal's /files/* + /ports contract so the
# OWUI terminal UI file browser connects unchanged. All paths are confined to the
# per-request workspace base (WORKDIR, or WORKDIR/<subdir> under X-Workspace-Subdir)
# via _safe_path — no traversal escape.


class CwdRequest(BaseModel):
    path: str


class WriteRequest(BaseModel):
    path: str
    content: str


class PathRequest(BaseModel):
    path: str


class MoveRequest(BaseModel):
    source: str
    destination: str


class ReplacementChunk(BaseModel):
    target: str
    replacement: str
    start_line: Optional[int] = Field(default=None, ge=1)
    end_line: Optional[int] = Field(default=None, ge=1)
    allow_multiple: bool = False


class ReplaceRequest(BaseModel):
    path: str
    replacements: list[ReplacementChunk]


class ArchiveRequest(BaseModel):
    paths: list[str]


@app.get("/ports")
async def list_ports():
    # Restricted runtime: no host-port introspection. Surface an empty list so the
    # UI ports panel renders cleanly (matches open-terminal's restricted fallback).
    return {"ports": []}


@app.get("/files/cwd")
async def get_cwd(subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    return {"cwd": base, "home": base}


@app.post("/files/cwd")
async def set_cwd(req: CwdRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    resolved = _safe_path(req.path, _request_base(subdir))
    if not os.path.isdir(resolved):
        raise HTTPException(status_code=404, detail="Directory not found")
    return {"cwd": resolved}


def _entry(p: str) -> Optional[dict]:
    try:
        st = os.stat(p)
        return {
            "name": os.path.basename(p),
            "type": "directory" if os.path.isdir(p) else "file",
            "size": int(st.st_size),
            "modified": float(st.st_mtime),
        }
    except OSError:
        return None  # file vanished between listdir and stat (TOCTOU race)


@app.get("/files/list")
async def list_dir(directory: str = ".", subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    if str(directory).strip().lower() in ("", "null"):
        directory = "."
    resolved = _safe_path(directory, _request_base(subdir))
    if not os.path.isdir(resolved):
        raise HTTPException(status_code=404, detail="Directory not found")
    try:
        entries = [e for e in (_entry(os.path.join(resolved, n)) for n in sorted(os.listdir(resolved))) if e is not None]
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"list failed: {e}") from e
    return {"dir": resolved, "entries": entries}


@app.get("/files/read")
async def read_file(path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    return await asyncio.to_thread(_read_file_impl, path, subdir)


def _read_file_impl(path: str, subdir: Optional[str]):
    full = _safe_path(path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    mime, _ = mimetypes.guess_type(full)
    if mime and mime.startswith("image/"):
        try:
            with open(full, "rb") as f:
                return Response(content=f.read(), media_type=mime)
        except OSError as e:
            raise HTTPException(status_code=500, detail=f"read failed: {e}") from e
    try:
        with open(full, "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"read failed: {e}") from e
    return {"path": full, "total_lines": len(content.splitlines()), "content": content}


@app.post("/files/write")
async def write_file(req: WriteRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    full = _safe_path(req.path, base)
    data = req.content.encode("utf-8")
    try:
        os.makedirs(os.path.dirname(full) or base, exist_ok=True)
        with open(full, "wb") as f:
            f.write(data)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "size": len(data)}


@app.post("/files/mkdir")
async def mkdir(req: PathRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(req.path, _request_base(subdir))
    try:
        os.makedirs(full, exist_ok=True)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full}


@app.post("/files/move")
async def move(req: MoveRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    src = _safe_path(req.source, base)
    dst = _safe_path(req.destination, base)
    if not os.path.exists(src):
        raise HTTPException(status_code=404, detail="Source path not found")
    if os.path.exists(dst):
        raise HTTPException(status_code=409, detail="Destination already exists")
    try:
        shutil.move(src, dst)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"source": src, "destination": dst}


@app.delete("/files/delete")
async def delete_entry(path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(path, _request_base(subdir))
    if not os.path.exists(full):
        raise HTTPException(status_code=404, detail="Path not found")
    is_dir = os.path.isdir(full)
    try:
        shutil.rmtree(full) if is_dir else os.remove(full)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "type": "directory" if is_dir else "file"}


@app.post("/files/replace")
async def replace(req: ReplaceRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(req.path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    try:
        with open(full, "r", encoding="utf-8", errors="replace") as f:
            content = f.read()
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    for chunk in req.replacements:
        content = _apply_replacement(content, chunk)
    data = content.encode("utf-8")
    try:
        with open(full, "wb") as f:
            f.write(data)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "size": len(data)}


def _replace_target(segment: str, chunk: ReplacementChunk) -> str:
    count = segment.count(chunk.target)
    if count == 0:
        raise HTTPException(status_code=400, detail=f"Target string not found: {chunk.target}")
    if count > 1 and not chunk.allow_multiple:
        raise HTTPException(status_code=400, detail=f"Found {count} occurrences of target but allow_multiple is false")
    if chunk.allow_multiple:
        return segment.replace(chunk.target, chunk.replacement)
    return segment.replace(chunk.target, chunk.replacement, 1)


def _apply_replacement(content: str, chunk: ReplacementChunk) -> str:
    if chunk.start_line is None and chunk.end_line is None:
        return _replace_target(content, chunk)
    lines = content.split("\n")
    start = max(0, (chunk.start_line or 1) - 1)
    end = len(lines) if chunk.end_line is None else min(len(lines), chunk.end_line)
    if start >= end:
        return content
    segment = "\n".join(lines[start:end])
    new_segment = _replace_target(segment, chunk)
    # rebuild via concatenation (avoids list slice-assignment type confusion)
    lines = lines[:start] + new_segment.split("\n") + lines[end:]
    return "\n".join(lines)


def _walk_files(root: str, include: Optional[list[str]] = None) -> list[str]:
    """All regular files under `root` (sorted); optional fnmatch include filter."""
    if os.path.isfile(root):
        return [root]
    out = []
    for dirpath, _dirnames, filenames in os.walk(root):
        for fn in filenames:
            if include and not any(fnmatch.fnmatch(fn, pat) for pat in include):
                continue
            out.append(os.path.join(dirpath, fn))
    out.sort()
    return out


@app.get("/files/grep")
async def grep(
    query: str,
    path: str = ".",
    regex: bool = True,
    case_insensitive: bool = False,
    include: Optional[list[str]] = Query(default=None),
    max_results: int = Query(default=50, ge=1, le=500),
    subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir"),
):
    return await asyncio.to_thread(
        _grep_impl, query, path, regex, case_insensitive, include, max_results, subdir
    )


def _grep_impl(query, path, regex, case_insensitive, include, max_results, subdir) -> dict:
    resolved = _safe_path(path, _request_base(subdir))
    if not os.path.exists(resolved):
        raise HTTPException(status_code=404, detail="Search path not found")
    flags = re.IGNORECASE if case_insensitive else 0
    try:
        pattern = re.compile(query, flags) if regex else re.compile(re.escape(query), flags)
    except re.error as e:
        raise HTTPException(status_code=400, detail=f"Invalid regex: {e}") from e
    matches: list[dict] = []
    for fpath in _walk_files(resolved, include):
        try:
            with open(fpath, "r", encoding="utf-8", errors="replace") as f:
                for lineno, line in enumerate(f, 1):
                    if pattern.search(line.rstrip("\n")):
                        matches.append({"file": fpath, "line": lineno, "content": line.rstrip("\n")})
                        if len(matches) >= max_results:
                            return {"query": query, "path": resolved, "matches": matches, "truncated": True}
        except OSError:
            continue
    return {"query": query, "path": resolved, "matches": matches, "truncated": False}


@app.get("/files/glob")
async def glob_search(
    pattern: str,
    path: str = ".",
    type: str = "any",
    max_results: int = Query(default=50, ge=1, le=500),
    subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir"),
):
    return await asyncio.to_thread(_glob_impl, pattern, path, type, max_results, subdir)


def _glob_impl(pattern, path, type, max_results, subdir) -> dict:
    resolved = _safe_path(path, _request_base(subdir))
    if not os.path.exists(resolved):
        raise HTTPException(status_code=404, detail="Search directory not found")
    matches: list[dict] = []
    for dirpath, dirnames, filenames in os.walk(resolved):
        for name in list(dirnames) + list(filenames):
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, resolved)
            if not (fnmatch.fnmatch(rel, pattern) or fnmatch.fnmatch(name, pattern)):
                continue
            is_dir = os.path.isdir(full)
            if type == "file" and is_dir:
                continue
            if type == "directory" and not is_dir:
                continue
            try:
                st = os.stat(full)
                matches.append({"path": rel, "type": "directory" if is_dir else "file", "size": int(st.st_size), "modified": float(st.st_mtime)})
            except OSError:
                continue
            if len(matches) >= max_results:
                matches.sort(key=lambda m: m["path"])
                return {"pattern": pattern, "path": resolved, "matches": matches, "truncated": True}
    matches.sort(key=lambda m: m["path"])
    return {"pattern": pattern, "path": resolved, "matches": matches, "truncated": False}


@app.post("/files/upload")
async def upload(
    file: UploadFile = File(...),
    directory: str = "",
    subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir"),
):
    base = _request_base(subdir)
    target_dir = base if str(directory).strip().lower() in ("", "null") else _safe_path(directory, base)
    if not os.path.isdir(target_dir):
        try:
            os.makedirs(target_dir, exist_ok=True)
        except OSError as e:
            raise HTTPException(status_code=400, detail=str(e)) from e
    filename = os.path.basename(file.filename or "upload")
    full = os.path.realpath(os.path.join(target_dir, filename))
    if full != base and not full.startswith(base + os.sep):
        raise HTTPException(status_code=400, detail="path escapes workspace")
    try:
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "size": os.path.getsize(full)}


@app.post("/files/archive")
async def archive(req: ArchiveRequest, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    return await asyncio.to_thread(_archive_impl, req, subdir)


def _archive_impl(req: ArchiveRequest, subdir: Optional[str]) -> Response:
    base = _request_base(subdir)
    if not req.paths:
        raise HTTPException(status_code=400, detail="No paths provided")
    resolved = []
    for p in req.paths:
        full = _safe_path(p, base)
        if not os.path.exists(full):
            raise HTTPException(status_code=404, detail=f"Path not found: {p}")
        resolved.append(full)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        for full in resolved:
            arcroot = os.path.basename(full.rstrip("/"))
            if os.path.isdir(full):
                for dirpath, _dn, filenames in os.walk(full):
                    for fn in filenames:
                        fp = os.path.join(dirpath, fn)
                        zf.write(fp, os.path.join(arcroot, os.path.relpath(fp, full)))
            else:
                zf.write(full, arcroot)
    name = os.path.basename(resolved[0].rstrip("/")) if len(resolved) == 1 else "download"
    return Response(
        content=buf.getvalue(),
        media_type="application/zip",
        headers={"Content-Disposition": f'attachment; filename="{name}.zip"'},
    )


# --- LLM-tool surface (openapi.json operationIds) ----------------------------
# Thin handlers backing the broker's curated openapi.json so the model's
# upload_file / download_file / list_files / check_exists tools resolve. These
# coexist with the open-terminal /files/* surface above (used by the terminal UI).


@app.post("/upload")
async def tool_upload(file: UploadFile = File(...), subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    filename = os.path.basename(file.filename or "upload")
    full = os.path.realpath(os.path.join(base, filename))
    if full != base and not full.startswith(base + os.sep):
        raise HTTPException(status_code=400, detail="path escapes workspace")
    try:
        n = 0
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
                n += len(chunk)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"saved": full, "bytes": n}


@app.get("/download/{file_path:path}")
async def tool_download(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    mime, _ = mimetypes.guess_type(full)
    return FileResponse(full, media_type=mime or "application/octet-stream", filename=os.path.basename(full))


@app.get("/list/{file_path:path}")
async def tool_list(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    fp = file_path.strip() or "."
    resolved = _safe_path(fp, _request_base(subdir))
    if not os.path.isdir(resolved):
        raise HTTPException(status_code=404, detail="Directory not found")
    entries = []
    try:
        for n in sorted(os.listdir(resolved)):
            p = os.path.join(resolved, n)
            try:
                st = os.stat(p)
                entries.append({"name": n, "is_dir": os.path.isdir(p), "size": int(st.st_size)})
            except OSError:
                continue
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"list failed: {e}") from e
    return {"path": resolved, "entries": entries}


@app.get("/exists/{file_path:path}")
async def tool_exists(file_path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path.strip() or ".", _request_base(subdir))
    return {"exists": os.path.exists(full), "is_file": os.path.isfile(full), "is_dir": os.path.isdir(full)}


@app.get("/files/view")
async def files_view(path: str, subdir: Optional[str] = Header(default=None, alias="X-Workspace-Subdir")):
    """Raw file bytes for download/display (OWUI downloadFileBlob -> res.blob())."""
    full = _safe_path(path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    mime, _ = mimetypes.guess_type(full)
    return FileResponse(full, media_type=mime or "application/octet-stream", filename=os.path.basename(full))

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

    async def _receiver() -> None:
        try:
            while not stop.is_set():
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
        except (WebSocketDisconnect, Exception):
            pass

    reader = asyncio.create_task(_pty_reader())
    receiver = asyncio.create_task(_receiver())
    try:
        # End as soon as EITHER the client goes (receiver) or the PTY/stream ends
        # (reader). Either way the finally block runs _term_cleanup so the PTY is
        # killed instead of leaking until the per-pod cap (-> 429).
        await asyncio.wait({reader, receiver}, return_when=asyncio.FIRST_COMPLETED)
    finally:
        stop.set()
        for t in (reader, receiver):
            t.cancel()
        with contextlib.suppress(Exception):
            await ws.close()
        await asyncio.gather(reader, receiver, return_exceptions=True)
        _term_cleanup(session_id)
