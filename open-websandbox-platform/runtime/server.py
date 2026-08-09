# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

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
 * fail-closed PER-SESSION API KEY: the broker injects a per-session Secret as a
   projected volume (/etc/runtime-key/api-key); the runtime refuses to boot on an
   unset/placeholder key and authenticates every hop per-session (issue #4).
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
import shlex
import shutil
import signal
import struct
import subprocess
import termios
import urllib.parse
import uuid as _uuid
import zipfile

from fastapi import (
    FastAPI,
    File,
    Header,
    HTTPException,
    Query,
    Request,
    Security,
    UploadFile,
    WebSocket,
    WebSocketDisconnect,
)
from fastapi.responses import FileResponse, Response, StreamingResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from prometheus_client import CONTENT_TYPE_LATEST, Counter, generate_latest
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
except (ValueError, OSError, AttributeError):  # pragma: no cover - rlimit best-effort; skip if unsupported
    pass

WORKDIR = os.environ.get("WORKDIR", "/workspace")
MAX_OUT = _env_int("MAX_OUTPUT_BYTES", 1 << 20)  # 1 MiB / stream
DEFAULT_TIMEOUT = _env_int("DEFAULT_TIMEOUT", 120)
MAX_TIMEOUT = _env_int("MAX_TIMEOUT", 600)
# S3-tiered offload/restore (issue #52): the broker streams a zstd tar of the whole
# workspace to/from S3. MAX_WORKSPACE_BYTES caps both directions (fail-on-exceed, D9) —
# the snapshot pre-check refuses before streaming; the restore caps the incoming stream.
# Default 2Gi matches broker.s3.sizeLimit (the emptyDir sizeLimit).
SNAPSHOT_MAX_BYTES = _env_int("MAX_WORKSPACE_BYTES", 2 * 1024 ** 3)  # 2 GiB (D9)

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("code-standard")

app = FastAPI(
    title="code-standard runtime",
    description="Exec + file API for an agent sandbox (OWUI open-terminal surface).",
)


# --- observability: Prometheus metrics (open_websandbox_ prefix) --------------
# A per-method/per-status request counter (scraped via /metrics), mirroring the
# broker. Best-effort: even when a downstream handler raises we count a 500 so
# error spikes stay visible to the scraper.
_REQUESTS = Counter(
    "open_websandbox_runtime_http_requests_total",
    "Runtime HTTP requests handled",
    ["method", "status"],
)


@app.middleware("http")
async def _count_requests(request: Request, call_next):
    try:
        response = await call_next(request)
        _REQUESTS.labels(request.method, str(response.status_code)).inc()
        return response
    except Exception:
        _REQUESTS.labels(request.method, "500").inc()
        raise


@app.get("/metrics", include_in_schema=False)
async def metrics() -> Response:
    """Prometheus exposition (process + python runtime metrics + the request counter)."""
    return Response(generate_latest(), media_type=CONTENT_TYPE_LATEST)


# --- OpenTelemetry tracing (bring-your-own collector via OTLP) -------------------
# Optional/soft: a complete no-op when the OTel libraries are not importable OR
# OTEL_EXPORTER_OTLP_ENDPOINT is unset, so the runtime boots + serves regardless.
# When configured it auto-instruments FastAPI (server spans) so the broker->runtime
# hop lands as a child of the propagated trace context. No collector is deployed by
# default.
def _setup_telemetry(app_obj, service_name: str) -> None:
    """Configure OTel tracing against a bring-your-own OTLP collector.

    A no-op unless OTEL_EXPORTER_OTLP_ENDPOINT is set AND the opentelemetry-* packages
    are importable. Instruments the FastAPI app so inbound requests are traced; the
    /metrics scrape is excluded from spans.
    """
    endpoint = os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT")
    if not endpoint:
        return
    try:
        from opentelemetry import trace
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import (
            OTLPSpanExporter,
        )
        from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
        from opentelemetry.sdk.resources import Resource
        from opentelemetry.sdk.trace import TracerProvider
        from opentelemetry.sdk.trace.export import BatchSpanProcessor
    except ImportError:
        log.debug("OpenTelemetry libraries not installed; tracing disabled")
        return

    provider = TracerProvider(resource=Resource.create({
        "service.name": os.environ.get("OTEL_SERVICE_NAME", service_name),
        "service.namespace": "open-websandbox",
    }))
    # OTLPSpanExporter() honours OTEL_EXPORTER_OTLP_ENDPOINT / *_PROTOCOL per the OTel spec.
    provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(provider)
    FastAPIInstrumentor.instrument_app(app_obj, excluded_urls="metrics")
    log.info("OpenTelemetry tracing enabled -> %s (service=%s)", endpoint, service_name)


# Bootstrap at import: no-op in tests (no OTEL_EXPORTER_OTLP_ENDPOINT); active in
# production when the chart points the runtime at a collector (bundled or BYO).
_setup_telemetry(app, "open-websandbox-runtime")


class ExecuteRequest(BaseModel):
    command: str
    timeout: int | None = Field(default=None, ge=1, le=MAX_TIMEOUT)


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


def _request_base(subdir: str | None) -> str:
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


# --- fail-closed inter-component auth (per-session API key) -----------------
# Defined here, ABOVE the first endpoint, because the gated routes reference
# _auth_runtime in their decorator dependencies — FastAPI resolves Security(...) at
# decorator-application time (import), so the guard must already exist.
#
# PER-SESSION KEY (issue #4). Each sandbox pod gets its OWN broker<->runtime key,
# delivered as a projected Secret volume mounted at /etc/runtime-key/api-key by the
# broker (it mints the key, writes Secret owui-runtime-key-<sandbox>, and injects the
# volume into the per-session Sandbox podTemplate — see chart + broker/main.py). The
# runtime reads it from the FILE, NOT an env var / NOT the kube API: the runtime pod is
# network-isolated from the API server (default-deny NetworkPolicy, no automounted
# service-account token), so a projected volume is the only native, isolation-preserving
# delivery. One pod sees exactly one key (true per-pod scoping).
#
# _validate_runtime_config() refuses to boot on an unset/placeholder key (fail-closed
# startup guard); _auth_runtime DENIES ON UNSET/PLACEHOLDER too (503) so the request
# path is fail-closed independently of the lifespan/boot guard — a process that skipped
# the startup event still cannot serve a gated hop unauthenticated. A presented Bearer
# must match (constant-time), else 401. On a mismatch we reload the file once so
# rotate-on-resume (a freshly synced Secret) is honored without a restart.
#
# Gated surface: POST /execute, the entire /files/* FS surface
# (read/write/delete/archive/mkdir/move/replace/grep/glob/upload/view/cwd), and the
# terminal management endpoints (POST/GET/DELETE /api/terminals[/{id}]). The broker
# attaches Authorization: Bearer <per-session key> on every runtime hop (terminal +
# execute + files); without it these 401. The interactive WS (/api/terminals/{id}) is
# frame-authed inline AND gated by the POST that creates its session. Health (/) and
# /metrics stay open for kubelet / Prometheus scraping (no credential available).
RUNTIME_KEY_FILE = os.environ.get("RUNTIME_KEY_FILE", "/etc/runtime-key/api-key")
_PLACEHOLDER_KEYS = frozenset({
    "", "dev-shared-secret-change-me", "change-me", "changeme", "placeholder",
})
# mtime-cached read so a rotated Secret (kubelet re-syncs the projected volume, giving
# the file a new mtime) is picked up without a process restart; rotate-on-resume is a
# fresh pod (fresh process) anyway, so this mainly bounds in-place rotation latency.
_key_cache: dict[str, object] = {"mtime": -1.0, "value": ""}


def _load_session_key() -> str:
    """Read the per-session API key from the mounted Secret volume (fail-closed).

    Returns '' when the file is absent/empty/unreadable (a misconfiguration the boot
    guard and the request guard both treat as fail-closed: a pod whose Secret/volume is
    missing never serves an authenticated hop). mtime-cached so rotate-on-resume and any
    in-place Secret rotation are reflected without a restart."""
    try:
        st = os.stat(RUNTIME_KEY_FILE)
    except OSError:
        if _key_cache["mtime"] != -2.0:  # cache the "missing" state too
            _key_cache["mtime"] = -2.0
            _key_cache["value"] = ""
        return ""
    if st.st_mtime != _key_cache["mtime"]:
        try:
            with open(RUNTIME_KEY_FILE, "r", encoding="utf-8", errors="replace") as f:
                _key_cache["value"] = f.read().strip()
        except OSError:
            _key_cache["value"] = ""
        _key_cache["mtime"] = st.st_mtime
    return str(_key_cache["value"])


def _validate_runtime_config() -> None:
    """Fail-closed startup guard: refuse to run without a per-session key file.

    The per-session key is delivered as a projected Secret volume at RUNTIME_KEY_FILE
    by the broker (chart + broker/main.py). An absent/empty/placeholder file is a
    misconfiguration (missing volume or bad Secret), so we refuse to start rather than
    run open. Wired into the startup event; tested directly."""
    if _load_session_key() in _PLACEHOLDER_KEYS:
        raise RuntimeError(
            "per-session runtime API key is missing or a placeholder — refusing to "
            "start. The broker must inject a per-session Secret as the projected volume "
            f"at {RUNTIME_KEY_FILE} (volume 'runtime-key')."
        )


_runtime_bearer = HTTPBearer(auto_error=False)


def _auth_runtime(
    credentials: HTTPAuthorizationCredentials | None = Security(_runtime_bearer),
    _retried: bool = False,
) -> None:
    """Validate the per-session API key (constant-time). Deny-on-unset (defense-in-depth).

    An unset/placeholder key is a misconfiguration, NOT a "disabled" mode: we 503 here
    so the request path is fail-closed regardless of the startup boot guard / lifespan
    (a process that skipped the startup event still cannot serve a gated hop
    unauthenticated). _validate_runtime_config() makes the same check at boot. A
    presented Bearer must match the configured key (constant-time), else 401. On a
    mismatch we invalidate the cache and re-read once so rotate-on-resume (a freshly
    synced Secret) is honored without a restart."""
    key = _load_session_key()
    if key in _PLACEHOLDER_KEYS:
        raise HTTPException(status_code=503, detail="per-session runtime API key is not configured")
    if credentials and hmac.compare_digest(credentials.credentials.encode(), key.encode()):
        return
    if not _retried:  # reload once: a just-rotated key may not yet be cached
        _key_cache["mtime"] = -1.0
        return _auth_runtime(credentials, _retried=True)
    raise HTTPException(status_code=401, detail="invalid runtime api key")

@app.get("/")
async def health():
    return {"status": "ok", "runtime": "code-standard"}


@app.post("/execute", response_model=ExecuteResponse, dependencies=[Security(_auth_runtime)])
async def execute(req: ExecuteRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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
        proc = await asyncio.create_subprocess_shell(
            req.command,
            cwd=base,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            start_new_session=True,  # own process group -> tree-kill on timeout
        )
        try:
            out_b, err_b = await asyncio.wait_for(proc.communicate(), timeout=timeout)
            exit_code = proc.returncode if proc.returncode is not None else 0
        except TimeoutError:
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
    start_line: int | None = Field(default=None, ge=1)
    end_line: int | None = Field(default=None, ge=1)
    allow_multiple: bool = False


class ReplaceRequest(BaseModel):
    path: str
    replacements: list[ReplacementChunk]


class ArchiveRequest(BaseModel):
    paths: list[str]


@app.get("/ports", dependencies=[Security(_auth_runtime)])
async def list_ports():
    # Restricted runtime: no host-port introspection. Surface an empty list so the
    # UI ports panel renders cleanly (matches open-terminal's restricted fallback).
    return {"ports": []}


@app.get("/files/cwd", dependencies=[Security(_auth_runtime)])
async def get_cwd(subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    return {"cwd": base, "home": base}


@app.post("/files/cwd", dependencies=[Security(_auth_runtime)])
async def set_cwd(req: CwdRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    resolved = _safe_path(req.path, _request_base(subdir))
    if not os.path.isdir(resolved):
        raise HTTPException(status_code=404, detail="Directory not found")
    return {"cwd": resolved}


def _entry(p: str) -> dict | None:
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


@app.get("/files/list", dependencies=[Security(_auth_runtime)])
async def list_dir(directory: str = ".", subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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


@app.get("/files/read", dependencies=[Security(_auth_runtime)])
async def read_file(path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    return await asyncio.to_thread(_read_file_impl, path, subdir)


def _read_file_impl(path: str, subdir: str | None):
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


@app.post("/files/write", dependencies=[Security(_auth_runtime)])
async def write_file(req: WriteRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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


@app.post("/files/mkdir", dependencies=[Security(_auth_runtime)])
async def mkdir(req: PathRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(req.path, _request_base(subdir))
    try:
        os.makedirs(full, exist_ok=True)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full}


@app.post("/files/move", dependencies=[Security(_auth_runtime)])
async def move(req: MoveRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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


@app.delete("/files/delete", dependencies=[Security(_auth_runtime)])
async def delete_entry(path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(path, _request_base(subdir))
    if not os.path.exists(full):
        raise HTTPException(status_code=404, detail="Path not found")
    is_dir = os.path.isdir(full)
    try:
        shutil.rmtree(full) if is_dir else os.remove(full)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "type": "directory" if is_dir else "file"}


@app.post("/files/replace", dependencies=[Security(_auth_runtime)])
async def replace(req: ReplaceRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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


def _walk_files(root: str, include: list[str] | None = None) -> list[str]:
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


@app.get("/files/grep", dependencies=[Security(_auth_runtime)])
async def grep(
    query: str,
    path: str = ".",
    regex: bool = True,
    case_insensitive: bool = False,
    include: list[str] | None = Query(default=None),
    max_results: int = Query(default=50, ge=1, le=500),
    subdir: str | None = Header(default=None, alias="X-Workspace-Subdir"),
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


@app.get("/files/glob", dependencies=[Security(_auth_runtime)])
async def glob_search(
    pattern: str,
    path: str = ".",
    type: str = "any",
    max_results: int = Query(default=50, ge=1, le=500),
    subdir: str | None = Header(default=None, alias="X-Workspace-Subdir"),
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


@app.post("/files/upload", dependencies=[Security(_auth_runtime)])
async def upload(
    file: UploadFile = File(...),
    directory: str = "",
    subdir: str | None = Header(default=None, alias="X-Workspace-Subdir"),
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
        raise HTTPException(status_code=400, detail="path escapes workspace")  # pragma: no cover - defense-in-depth: os.path.basename already strips separators
    try:
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"path": full, "size": os.path.getsize(full)}


@app.post("/files/archive", dependencies=[Security(_auth_runtime)])
async def archive(req: ArchiveRequest, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    return await asyncio.to_thread(_archive_impl, req, subdir)


def _archive_impl(req: ArchiveRequest, subdir: str | None) -> Response:
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


@app.post("/upload", dependencies=[Security(_auth_runtime)])
async def tool_upload(file: UploadFile = File(...), subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    base = _request_base(subdir)
    filename = os.path.basename(file.filename or "upload")
    full = os.path.realpath(os.path.join(base, filename))
    if full != base and not full.startswith(base + os.sep):
        raise HTTPException(status_code=400, detail="path escapes workspace")  # pragma: no cover - defense-in-depth: os.path.basename already strips separators
    try:
        n = 0
        with open(full, "wb") as f:
            while chunk := await file.read(1 << 20):
                f.write(chunk)
                n += len(chunk)
    except OSError as e:
        raise HTTPException(status_code=400, detail=str(e)) from e
    return {"saved": full, "bytes": n}


@app.get("/download/{file_path:path}", dependencies=[Security(_auth_runtime)])
async def tool_download(file_path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    mime, _ = mimetypes.guess_type(full)
    return FileResponse(full, media_type=mime or "application/octet-stream", filename=os.path.basename(full))


@app.get("/list/{file_path:path}", dependencies=[Security(_auth_runtime)])
async def tool_list(file_path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
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


@app.get("/exists/{file_path:path}", dependencies=[Security(_auth_runtime)])
async def tool_exists(file_path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    full = _safe_path(file_path.strip() or ".", _request_base(subdir))
    return {"exists": os.path.exists(full), "is_file": os.path.isfile(full), "is_dir": os.path.isdir(full)}


@app.get("/files/view", dependencies=[Security(_auth_runtime)])
async def files_view(path: str, subdir: str | None = Header(default=None, alias="X-Workspace-Subdir")):
    """Raw file bytes for download/display (OWUI downloadFileBlob -> res.blob())."""
    full = _safe_path(path, _request_base(subdir))
    if not os.path.isfile(full):
        raise HTTPException(status_code=404, detail="File not found")
    mime, _ = mimetypes.guess_type(full)
    return FileResponse(full, media_type=mime or "application/octet-stream", filename=os.path.basename(full))

# --- S3-tiered snapshot/restore (broker-orchestrated offload/restore, issue #52) ------
# The broker is the sole S3 client (#50 network isolation preserved): it streams a
# zstd-compressed tar of the WHOLE workspace off (GET /snapshot) and back on
# (PUT /restore) over the same per-session key as /execute + /files/*. The broker
# attaches the Bearer via its per-session auth header.


def _workspace_size_bytes(base: str) -> int:
    """Apparent bytes of everything under `base` (snapshot pre-check, fail-on-exceed D9)."""
    total = 0
    for root, _dirs, files in os.walk(base):
        for fn in files:
            try:
                total += os.path.getsize(os.path.join(root, fn))
            except OSError:
                continue
    return total


async def _snapshot_chunks(base: str):
    """Stream a zstd-compressed tar of `base`'s CONTENTS (no leading '.' entry) in chunks.

    Uses `cd <base> && find . -mindepth 1 | tar --no-recursion` so the archive never
    contains a '.' entry. That matters because PUT /restore runs as a non-root uid
    against a root-owned emptyDir mount point: restoring a '.' entry would make tar try
    to set the mount point's mode/mtime, which only the owner (root) may do, failing
    the restore with exit 2. Streaming the contents (each entry listed by find) avoids
    touching the mount point at all. Honors SNAPSHOT_MAX_BYTES only via the pre-check."""
    cmd = (f"cd {shlex.quote(base)} && find . -mindepth 1 -print0 "
           f"| tar --null --no-recursion -cf - -T - | zstd -3 -q")
    proc = await asyncio.create_subprocess_exec(
        "sh", "-c", cmd,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
    )
    assert proc.stdout is not None
    try:
        while True:
            chunk = await proc.stdout.read(1 << 20)
            if not chunk:
                break
            yield chunk
    finally:
        if proc.returncode is None:
            proc.kill()
        rc = await proc.wait()
        if rc not in (0, None):
            err = await proc.stderr.read() if proc.stderr else b""
            log.warning("snapshot pipeline rc=%s err=%s", rc, err[:200].decode("utf-8", "replace"))


@app.get("/snapshot", dependencies=[Security(_auth_runtime)])
async def snapshot():
    """Stream a zstd-compressed tar of the whole workspace for S3 offload (issue #52).

    Pre-checks the logical workspace size against SNAPSHOT_MAX_BYTES (D9 fail-on-exceed,
    413 BEFORE streaming) so the broker never uploads a workspace larger than the
    configured hot-tier sizeLimit."""
    base = _request_base(None)
    if _workspace_size_bytes(base) > SNAPSHOT_MAX_BYTES:
        raise HTTPException(status_code=413, detail="workspace exceeds MAX_WORKSPACE_BYTES")
    return StreamingResponse(
        _snapshot_chunks(base),
        media_type="application/zstd",
        headers={"Content-Disposition": "attachment; filename=\"workspace.tar.zst\""},
    )


@app.put("/restore", dependencies=[Security(_auth_runtime)])
async def restore(request: Request):
    """Accept a zstd-compressed tar (from S3) streamed into the whole workspace.

    Caps the incoming COMPRESSED stream at SNAPSHOT_MAX_BYTES (D9 fail-on-exceed, 413);
    the emptyDir sizeLimit is the uncompressed backstop (kubelet eviction). Streams
    `zstd -d | tar -xf - -C <base>` so it round-trips GET /snapshot."""
    base = _request_base(None)
    proc = await asyncio.create_subprocess_exec(
        "sh", "-c", f"zstd -d -q | tar -xf - -C {shlex.quote(base)}",
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    assert proc.stdin is not None
    received = 0
    exceeded = False
    try:
        async for chunk in request.stream():
            received += len(chunk)
            if received > SNAPSHOT_MAX_BYTES:
                exceeded = True
                break
            try:
                proc.stdin.write(chunk)
                await proc.stdin.drain()
            except (BrokenPipeError, ConnectionResetError):
                break  # pipeline exited early on bad input; reported via rc below
    finally:
        # Closing stdin gives the zstd|tar pipeline EOF so it terminates naturally;
        # proc.kill()+proc.wait() can wedge under coverage's subprocess tracing, and
        # EOF-exit is reliable (zstd -d always exits once its stdin closes).
        with contextlib.suppress(Exception):
            proc.stdin.close()
        if exceeded:
            await proc.wait()
            raise HTTPException(
                status_code=413,
                detail=f"restore stream exceeds MAX_WORKSPACE_BYTES ({SNAPSHOT_MAX_BYTES})",
            )
        rc = await proc.wait()
        if rc != 0:
            err = await proc.stderr.read() if proc.stderr else b""
            log.error("restore pipeline failed rc=%s base=%s stderr=%s", rc, base, err[:300].decode('utf-8', 'replace'))
            raise HTTPException(
                status_code=500,
                detail=f"restore pipeline failed rc={rc}: {err[:200].decode('utf-8', 'replace')}",
            )
    log.info("restore into %s: %d bytes", base, received)
    return {"restored": True, "bytes": received}


# --- interactive terminal (PTY) -------------------------------------------------
# open-terminal-compatible /api/terminals surface so OWUI's terminal UI connects
# unchanged. POST forks a shell on a PTY scoped to the chat workspace folder; the WS
# at /api/terminals/{id} streams BINARY stdin/stdout with TEXT
# {"type":"resize","cols":N,"rows":N} control frames. Inter-component auth is
# fail-closed: _validate_runtime_config() refuses to boot without a per-session key,
# POST /api/terminals requires a matching Bearer, and an optional first
# {"type":"auth","token":...} WS frame is validated inline in _receiver (the broker
# consumes OWUI's frame upstream and forwards raw bytes, so the frame is not required).
# Verified under gVisor runsc: openpty/fork/ioctl TIOCSWINSZ/select are all emulated.
# One terminal per chat (session id = X-Session-Id); destroyed on WS close.

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
    # master_fd is O_NONBLOCK: write best-effort, never block the caller. A full pty
    # input buffer raises BlockingIOError (subclass of OSError) -> drop the remainder
    # (rare; only on very large pastes).
    mv = memoryview(data)
    sent = 0
    while sent < len(mv):
        try:
            n = os.write(master_fd, mv[sent:])
        except OSError:
            break  # pty input buffer full (BlockingIOError) or closed — drop remainder
        if n <= 0:  # pragma: no cover - os.write to a live pty master returns >0; OSError handled above
            break
        sent += n


@app.post("/api/terminals")
async def create_terminal(
    subdir: str | None = Header(default=None, alias="X-Workspace-Subdir"),
    session_id: str | None = Header(default=None, alias="X-Session-Id"),
    _=Security(_auth_runtime),
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
        proc = subprocess.Popen(
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


@app.get("/api/terminals", dependencies=[Security(_auth_runtime)])
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


@app.get("/api/terminals/{session_id}", dependencies=[Security(_auth_runtime)])
async def get_terminal(session_id: str):
    s = _terminals.get(session_id)
    if not s or not _term_alive(s):
        if session_id in _terminals:
            _term_cleanup(session_id)
        raise HTTPException(status_code=404, detail="terminal not found")
    return {"id": session_id, "created_at": s["created_at"], "pid": s["proc"].pid}


@app.delete("/api/terminals/{session_id}", dependencies=[Security(_auth_runtime)])
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
    # Inter-component auth runs inline in _receiver (an optional first
    # {"type":"auth","token":...} TEXT frame); the broker forwards raw bytes and never
    # sends one, so we must not require it up-front (broker-compat).

    master_fd, proc = s["master_fd"], s["proc"]
    stop = asyncio.Event()
    loop = asyncio.get_event_loop()
    out_q: asyncio.Queue = asyncio.Queue()

    def _on_pty_readable() -> None:
        # add_reader callback: epoll-driven, runs on the loop thread, never blocks.
        # Drain everything currently available then return — level-triggered epoll
        # re-fires when more PTY output arrives. ZERO threads (vs the old
        # run_in_executor + select(0.1) poll loop that pinned a worker per terminal).
        while True:
            try:
                data = os.read(master_fd, 4096)
            except OSError as e:
                if isinstance(e, BlockingIOError):
                    return  # drained to EAGAIN; re-triggered when more data arrives
                out_q.put_nowait(None)  # PTY closed / child gone (EIO on Linux)
                return
            if not data:  # pragma: no cover - EOF on child exit; subprocess spawn is bounded by the test-env rlimit, exercised in e2e
                out_q.put_nowait(None)  # EOF
                return
            out_q.put_nowait(data)

    loop.add_reader(master_fd, _on_pty_readable)

    async def _pty_reader() -> None:
        try:
            while not stop.is_set():  # pragma: no branch - graceful stop-set exit exercised in e2e
                try:
                    data = await asyncio.wait_for(out_q.get(), timeout=1.0)
                except TimeoutError:
                    # Heartbeat proc-death check (no thread, no blocking): catches the
                    # edge case where the shell exits but a background job still holds
                    # the slave open (so no EOF/EIO arrives on the master fd).
                    if proc.poll() is not None:  # pragma: no cover - child death w/ bg job holding slave (no EOF); exercised in e2e
                        break
                    continue
                if data is None:
                    break  # EOF / EIO sentinel from the callback
                try:
                    await ws.send_bytes(data)
                except Exception:  # pragma: no cover - client gone mid-send; exercised in e2e
                    break
        finally:
            loop.remove_reader(master_fd)

    async def _receiver() -> None:
        try:
            while not stop.is_set():  # pragma: no branch - graceful stop-set exit exercised in e2e
                msg = await ws.receive()
                if msg["type"] == "websocket.disconnect":
                    break
                if msg.get("bytes"):
                    # master_fd is O_NONBLOCK -> _term_write cannot block the loop.
                    _term_write(master_fd, msg["bytes"])
                elif msg.get("text"):
                    try:
                        payload = json.loads(msg["text"])
                    except (json.JSONDecodeError, ValueError, TypeError):
                        log.debug("ignoring malformed terminal control message")
                        continue
                    ptype = payload.get("type") if isinstance(payload, dict) else None
                    if ptype == "auth":
                        # Fail-closed inter-component auth (constant-time). The broker
                        # consumes OWUI's auth frame upstream and forwards raw bytes, so a
                        # direct client's auth frame is validated HERE, inline. A wrong
                        # token (or any token while the key is unset) tears the session
                        # down (4001). The per-session key is guaranteed set at boot
                        # _validate_runtime_config(); the unset case only arises in dev.
                        if not hmac.compare_digest(str(payload.get("token", "")), _load_session_key()):
                            await ws.close(code=4001, reason="invalid api key")
                            break
                    elif ptype == "resize":
                        try:
                            rows, cols = int(payload.get("rows", 24)), int(payload.get("cols", 80))
                        except (TypeError, ValueError):
                            rows, cols = 24, 80  # tolerate a malformed resize frame
                        with contextlib.suppress(OSError):
                            fcntl.ioctl(master_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        except (WebSocketDisconnect, Exception):  # pragma: no cover - receiver fault/disconnect; exercised in e2e
            log.debug("terminal receiver ended")

    reader = asyncio.create_task(_pty_reader())
    receiver = asyncio.create_task(_receiver())
    try:
        # End as soon as EITHER the client goes (receiver) or the PTY/stream ends
        # (reader). Either way the finally block runs _term_cleanup so the PTY is
        # killed instead of leaking until the per-pod cap (-> 429).
        await asyncio.wait({reader, receiver}, return_when=asyncio.FIRST_COMPLETED)
    finally:
        stop.set()
        loop.remove_reader(master_fd)  # idempotent; stops further callbacks
        for t in (reader, receiver):
            t.cancel()
        with contextlib.suppress(Exception):
            await ws.close()
        await asyncio.gather(reader, receiver, return_exceptions=True)
        _term_cleanup(session_id)


@app.on_event("startup")
async def _validate_on_startup() -> None:
    """Fail-closed boot guard: refuse to serve without a per-session key file.

    Mirrors the broker's startup _validate_config(). Local dev/tests bypass this
    (uvicorn lifespan="off" / the in-process ASGI transport never fire startup events);
    production uvicorn runs lifespan on, so a misconfigured deploy crashes fast instead
    of running open."""
    _validate_runtime_config()


@app.on_event("shutdown")
async def _on_shutdown() -> None:
    """Graceful SIGTERM shutdown: best-effort close of active PTY master fds.

    Per-connection WS reader/receiver tasks are torn down by the event loop when
    their sockets close; there are no global long-lived tasks here (unlike the
    broker's reaper). We only reap the tracked PTY sessions so SIGTERM doesn't
    orphan shell process groups / leak open master fds. Never blocks: per-terminal
    cleanup is synchronous and each failure is swallowed.
    """
    for sid in list(_terminals):
        with contextlib.suppress(Exception):
            _term_cleanup(sid)
