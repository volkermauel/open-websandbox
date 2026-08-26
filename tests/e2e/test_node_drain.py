"""Node-drain / pod-eviction terminal resilience (issue #129).

Opt-in (``E2E_DRAIN=1``) — needs a live broker + KIND cluster and exercises the
full drain story end to end:

1. Claim a **persistent** session (``X-Persistence: persistent``) and open a WS
   terminal through the broker; produce output and write a marker file.
2. Delete the sandbox pod (the same blast radius as a node drain — a real
   ``kubectl drain`` additionally cordons, which changes nothing for the pod).
3. The live WS dies (in-pod state is lost — documented behaviour).
4. Reconnecting through the broker re-resolves the recreated pod; the terminal
   resumes with the SIGTERM-flushed scrollback replayed and the marker file
   intact on the PVC.

What survives vs. what dies (documented in docs/operations.md):
  survives — PVC files, the scrollback tail (flushed on SIGTERM), the session id
  dies     — the shell process, its environment/cwd, running jobs

Prerequisites (same as the upgrade/rollback lane):
  * a chart install where persistent sessions can bind storage — on single-node
    KIND the default RWO ``standard`` class works (same-node reattach); cross-node
    drain needs an RWX class (see docs/deploy.md).
  * kubectl on PATH; honours R1 (explicit ``--kubeconfig`` when ``KUBECONFIG`` is
    set, never the ambient default).
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import time
from collections.abc import AsyncIterator

import httpx
import pytest
from conftest import BROKER_SECRET, BROKER_URL, TEST_USER, headers_for

pytest.importorskip("websockets")

RT_NS = os.getenv("E2E_RT_NS", "agent-sandbox-runtime")
CLAIM_TIMEOUT = float(os.getenv("E2E_DRAIN_CLAIM_TIMEOUT", "240"))
RECONNECT_TIMEOUT = float(os.getenv("E2E_DRAIN_RECONNECT_TIMEOUT", "240"))

pytestmark = pytest.mark.skipif(
    os.getenv("E2E_DRAIN") != "1",
    reason="opt-in: set E2E_DRAIN=1 (needs a live broker/cluster with PVC storage)",
)


def _run(cmd: list[str], timeout: float = 60) -> str:
    """subprocess.run with explicit R1 kubeconfig handling."""
    env = dict(os.environ)
    kc = env.get("KUBECONFIG")
    if kc and cmd[0] == "kubectl" and "--kubeconfig" not in cmd:
        cmd = ["kubectl", "--kubeconfig", kc, *cmd[1:]]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:  # pragma: no cover - diagnostics only
        pytest.fail(f"{' '.join(cmd)} timed out after {timeout}s")
    if r.returncode != 0:
        pytest.fail(f"{' '.join(cmd)} failed ({r.returncode}): {r.stderr.strip()[:400]}")
    return r.stdout


def _sandbox_name(user_id: str, session: str) -> str:
    """Mirror broker `sandbox_name` for the persistent profile."""
    digest = hashlib.sha256(f"{user_id}/{session}".encode()).hexdigest()[:12]
    return f"owui-c-{digest}"


def _sandbox_pod(session: str) -> str:
    prefix = _sandbox_name(TEST_USER, session)
    out = _run(
        ["kubectl", "get", "pods", "-n", RT_NS, "-o", "name"],
    )
    pods = [p.removeprefix("pod/") for p in out.split() if p.strip()]
    match = [p for p in pods if p.startswith(prefix)]
    assert match, f"no pod found for sandbox {prefix} in {RT_NS} (pods: {pods[:8]})"
    return match[0]


async def _claim_persistent(client: httpx.AsyncClient, session: str) -> None:
    """Claim a persistent session; skip the lane if storage can't bind."""
    hdr = {**headers_for(TEST_USER, session), "X-Persistence": "persistent"}
    deadline = time.monotonic() + CLAIM_TIMEOUT
    last = "no attempt"
    while time.monotonic() < deadline:
        try:
            r = await client.post(
                "/execute", json={"command": "echo ready"}, headers=hdr
            )
            if r.status_code == 200 and r.json().get("exit_code") == 0:
                return
            last = f"HTTP {r.status_code}: {r.text[:120]}"
        except Exception as exc:  # noqa: BLE001 — transient broker/proxy unavailability
            last = repr(exc)[:120]
        await __import__("asyncio").sleep(3)
    pytest.skip(
        f"persistent session never became ready in {CLAIM_TIMEOUT}s (last: {last}) — "
        "this lane needs PVC storage for the persistent profile (single-node KIND "
        "RWO works; see docs/deploy.md)"
    )


@pytest.fixture
async def ws_session() -> AsyncIterator[str]:
    """A claimed persistent session id (unique per run)."""
    session = f"drain-{int(time.time())}"
    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=30) as client:
        await _claim_persistent(client, session)
    yield session


async def _ws_connect(session: str):
    import asyncio

    import websockets

    url = f"{BROKER_URL.replace('http', 'ws')}/api/terminals/{session}"
    ws = await websockets.connect(
        url,
        additional_headers={**headers_for(TEST_USER, session),
                            "X-Persistence": "persistent"},
        open_timeout=20,
        close_timeout=5,
    )
    await ws.send(json.dumps({"type": "auth", "token": BROKER_SECRET}))
    await ws.send(json.dumps({"type": "resize", "rows": 24, "cols": 80}))
    # settle: drain the first prompt/scrollback frames
    await asyncio.sleep(0.5)
    return ws


async def _wait_marker(ws, marker: str, timeout: float = 60.0) -> None:
    """Read frames until `marker` appears in the binary output stream."""
    import asyncio

    deadline = time.monotonic() + timeout
    buf = b""
    while time.monotonic() < deadline:
        try:
            frame = await asyncio.wait_for(
                ws.recv(), timeout=max(0.1, deadline - time.monotonic())
            )
        except Exception as exc:  # noqa: BLE001 — socket death IS the signal here
            raise AssertionError(
                f"socket died before {marker!r} (got: {buf[-200:]!r}): {exc!r}"
            ) from exc
        if isinstance(frame, bytes):
            buf += frame
            if marker.encode() in buf:
                return
    raise AssertionError(f"{marker!r} never arrived (got: {buf[-200:]!r})")


@pytest.mark.asyncio
async def test_pod_eviction_resumes_terminal_files_and_scrollback(ws_session):
    session = ws_session
    marker_file = f"/workspace/drain-{int(time.time())}.txt"

    # 1. Marker file + terminal output while the pod is alive.
    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=30) as client:
        r = await client.post(
            "/execute",
            json={"command": f"echo survive > {marker_file}"},
            headers={**headers_for(TEST_USER, session), "X-Persistence": "persistent"},
        )
        assert r.status_code == 200, r.text
    ws = await _ws_connect(session)
    await ws.send(b"echo PRE_EVICT_MARKER\n")
    await _wait_marker(ws, "PRE_EVICT_MARKER")

    # 2. Evict the pod (same blast radius as node drain).
    pod = _sandbox_pod(session)
    _run(["kubectl", "delete", "pod", pod, "-n", RT_NS, "--grace-period=30"], timeout=90)

    # 3. The live WS dies — in-pod state does not survive eviction.
    with pytest.raises(Exception):  # noqa: B017, PT011 — any socket failure is fine
        await _wait_marker(ws, "NEVER_COMES", timeout=60)
    await ws.close()

    # 4. Reconnect: broker re-resolves the recreated pod, ensure_pty reuses the
    #    id, the flushed scrollback replays, and the file survived on the PVC.
    ws2 = None
    deadline = time.monotonic() + RECONNECT_TIMEOUT
    while ws2 is None and time.monotonic() < deadline:
        try:
            ws2 = await _ws_connect(session)
        except Exception:  # noqa: BLE001 — pod still coming up
            await __import__("asyncio").sleep(3)
    assert ws2 is not None, "could not reconnect after eviction"
    # The SIGTERM-flushed tail from the killed pod replays first.
    await _wait_marker(ws2, "PRE_EVICT_MARKER", timeout=30)
    # ... and the resumed terminal is live.
    await ws2.send(b"echo POST_EVICT_MARKER\n")
    await _wait_marker(ws2, "POST_EVICT_MARKER")

    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=30) as client:
        r = await client.post(
            "/execute",
            json={"command": f"cat {marker_file}"},
            headers={**headers_for(TEST_USER, session), "X-Persistence": "persistent"},
        )
        body = r.json()
    assert r.status_code == 200 and body.get("exit_code") == 0, r.text
    assert "survive" in body.get("stdout", ""), "marker file lost across eviction"

    await ws2.close()
