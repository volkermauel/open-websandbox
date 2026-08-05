"""Shared fixtures for runtime server unit tests.

The runtime server (``agent-sandbox-platform/runtime/server.py``) is a standalone
FastAPI app with no Kubernetes coupling, so it is fully unit-testable on a Linux
box with a real filesystem and a real PTY — no cluster, no mocking of those.

Two import-time side effects of ``server.py`` must be controlled here, *before*
the module is imported for the first time:

1. ``MAX_PROCS`` → ``resource.setrlimit(RLIMIT_NPROC, 256)``.
   On a dev machine the test user already owns far more than 256 processes/threads
   (each app thread counts toward RLIMIT_NPROC on Linux), so lowering the cap to
   256 makes every later ``fork()`` fail with ``EAGAIN`` — breaking ``/execute``,
   the PTY shell spawn, etc. We neutralise it by pointing ``MAX_PROCS`` at the
   current hard limit (a no-op setrlimit) instead of editing ``server.py``.

2. ``WORKDIR`` is left at its import default (``/workspace``); every test points
   it at its own ``tmp_path`` via the ``workdir`` fixture so nothing ever touches
   the real ``/workspace``.
"""

from __future__ import annotations

import os
import resource as _resource
import socket
import sys
import threading
import time
from collections.abc import AsyncIterator
from pathlib import Path

import httpx
import pytest

# --- (1) neutralise the RLIMIT_NPROC cap BEFORE importing server ---------------
# server.py does: setrlimit(RLIMIT_NPROC, (_env_int("MAX_PROCS", 256),)*2).
# Pin MAX_PROCS at the *current* hard limit so the setrlimit is a no-op and fork
# keeps working. This must run before the first `import server` anywhere.
_os_env_max_procs = str(_resource.getrlimit(_resource.RLIMIT_NPROC)[1])
os.environ.setdefault("MAX_PROCS", _os_env_max_procs)

# --- make the runtime package importable as top-level `server` -----------------
_RUNTIME_DIR = Path(__file__).resolve().parents[3] / "agent-sandbox-platform" / "runtime"
if str(_RUNTIME_DIR) not in sys.path:
    sys.path.insert(0, str(_RUNTIME_DIR))

import server  # type: ignore[import-not-found]  # noqa: E402  (runtime import; path inserted above)


@pytest.fixture
def workdir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> str:
    """An isolated workspace root; each test gets its own empty dir.

    ``server.WORKDIR`` is read at request time by ``_request_base`` (the only
    thing that feeds ``base`` into ``_safe_path``), so patching the module global
    here confines every endpoint to this tmp dir for the duration of the test.
    """
    wd = tmp_path / "workspace"
    wd.mkdir()
    monkeypatch.setattr(server, "WORKDIR", str(wd))
    return str(wd)


@pytest.fixture
async def client(workdir: str) -> AsyncIterator[httpx.AsyncClient]:
    """In-process async client over the FastAPI ASGI app (for HTTP endpoints)."""
    transport = httpx.ASGITransport(app=server.app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test", follow_redirects=True) as c:
        yield c


@pytest.fixture
def live_base(workdir: str):
    """Spin a real uvicorn server on an ephemeral port (for the WebSocket PTY test).

    httpx's ASGITransport cannot upgrade to WebSocket, so the terminal WS test
    needs an actual listening server. Returns ``(ws_base, http_base)`` URLs.
    Function-scoped so each test gets a fresh process and clean terminal state.
    """
    import uvicorn

    # Grab an ephemeral port from the OS, then hand it to uvicorn.
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()

    config = uvicorn.Config(
        server.app, host="127.0.0.1", port=port, log_level="warning", lifespan="off"
    )
    uvi = uvicorn.Server(config)
    thread = threading.Thread(target=uvi.run, daemon=True)
    thread.start()

    # Wait until the port actually accepts connections (uvicorn is listening).
    deadline = time.time() + 10.0
    ready = False
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                ready = True
                break
        except OSError:
            time.sleep(0.02)
    if not ready:
        uvi.should_exit = True
        thread.join(timeout=5)
        raise RuntimeError("uvicorn failed to start within 10s")

    ws_base = f"ws://127.0.0.1:{port}"
    http_base = f"http://127.0.0.1:{port}"
    try:
        yield ws_base, http_base
    finally:
        uvi.should_exit = True
        thread.join(timeout=5)


@pytest.fixture
def clean_terminals():
    """Ensure no leaked PTY sessions survive a terminal test (defensive teardown)."""
    yield
    for sid in list(server._terminals.keys()):
        with contextlib_suppress():
            server._term_cleanup(sid)


def contextlib_suppress():
    import contextlib

    return contextlib.suppress(Exception)
