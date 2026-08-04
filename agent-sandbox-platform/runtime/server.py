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
import contextlib
import logging
import os
import signal
import subprocess
import urllib.parse
from typing import Optional

from fastapi import FastAPI, File, HTTPException, UploadFile
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


def _safe_path(rel: str) -> str:
    """Resolve `rel` under WORKDIR, rejecting escapes (../, absolute, symlinks out)."""
    rel = urllib.parse.unquote(rel).lstrip("/")
    base = os.path.realpath(WORKDIR)
    full = os.path.realpath(os.path.join(base, rel))
    if full != base and not full.startswith(base + os.sep):
        raise HTTPException(status_code=400, detail="path escapes workspace")
    return full


@app.get("/")
async def health():
    return {"status": "ok", "runtime": "code-standard"}


@app.post("/execute", response_model=ExecuteResponse)
async def execute(req: ExecuteRequest):
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
            cwd=WORKDIR,
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
async def upload_file(file: UploadFile = File(...)):
    full = _safe_path(file.filename or "")
    try:
        os.makedirs(os.path.dirname(full) or WORKDIR, exist_ok=True)
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"write failed: {e}") from e
    return {"saved": os.path.relpath(full, WORKDIR), "bytes": os.path.getsize(full)}


@app.get("/download/{file_path:path}")
async def download_file(file_path: str):
    full = _safe_path(file_path)
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="not found")
    return FileResponse(full, filename=os.path.basename(full))


@app.get("/list/{file_path:path}")
async def list_files(file_path: str):
    full = _safe_path(file_path)
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
    return {"path": os.path.relpath(full, WORKDIR), "entries": entries}


@app.get("/exists/{file_path:path}")
async def exists(file_path: str):
    full = _safe_path(file_path)
    return {"exists": os.path.exists(full), "is_file": os.path.isfile(full), "is_dir": os.path.isdir(full)}
