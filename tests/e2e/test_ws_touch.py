"""#158: WS terminal traffic refreshes ``broker-last-used``.

Production trace (chat ``8606eb0b``): the relay started 09:03:58, the last
HTTP resolve touched ``broker-last-used`` 09:06:12, and the leader reaper
parked the **actively-used** sandbox 09:08:31 (``idle=139s`` > ``park_idle``
120s) — the pod delete killed the relay mid-session. Only HTTP resolves
refreshed the idle clock; the relay now refreshes it on relayed frames
(throttled by ``BROKER_WS_TOUCH_INTERVAL_SECONDS`` — the first frame always
touches, later ones at most once per interval).

Proves the live behavior end-to-end: claim a session (resolve touches the
annotation), read the stamp, drive real frames over the broker's WS relay,
then assert the stamp advanced — nothing but the relay could have written it.
"""

import asyncio
import hashlib
import json
import os
import subprocess
import time

import httpx
import pytest

pytest.importorskip("websockets")

from conftest import (  # noqa: E402
    BROKER_SECRET,
    BROKER_URL,
    CLAIM_TIMEOUT,
    TEST_USER,
    _claim_ready_session,
    headers_for,
)

RT_NS = os.getenv("E2E_RT_NS", "agent-sandbox-runtime")

# Reads the sandbox annotation via kubectl, so it needs a kubeconfig (every
# e2e lane exports one — R1 keeps it explicit).
pytestmark = pytest.mark.skipif(
    not os.getenv("KUBECONFIG"),
    reason="needs KUBECONFIG to read the sandbox annotation via kubectl",
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
    """Mirror the broker's `sandbox_name` for the persistent profile."""
    digest = hashlib.sha256(f"{user_id}/{session}".encode()).hexdigest()[:12]
    return f"owui-c-{digest}"


def _last_used(sandbox: str) -> int:
    out = _run(
        [
            "kubectl",
            "get",
            "sandbox",
            sandbox,
            "-n",
            RT_NS,
            "-o",
            "jsonpath={.metadata.annotations.broker-last-used}",
        ]
    )
    raw = out.strip()
    assert raw, f"no broker-last-used annotation on {sandbox}"
    return int(raw)


def test_ws_frames_refresh_last_used():
    session = f"ws-touch-{int(time.time())}"
    sandbox = _sandbox_name(TEST_USER, session)

    # 1. Claim via HTTP: the resolve touches the annotation (pre-#158 behavior
    #    — this was the ONLY writer, which is exactly the bug).
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
    base = _last_used(sandbox)
    assert base > 0

    # Guarantee a strictly later second so a refresh is observable even on a
    # fast machine (annotation granularity is whole seconds).
    time.sleep(2)

    # 2. Drive real frames through the broker's WS relay: auth (consumed by
    #    wait_auth) + a resize frame (relayed upstream → first-frame touch).
    async def drive_frames() -> None:
        import websockets

        url = f"{BROKER_URL.replace('http', 'ws')}/api/terminals/{session}"
        async with websockets.connect(
            url,
            additional_headers={
                **headers_for(TEST_USER, session),
                "X-Persistence": "persistent",
            },
            open_timeout=20,
            close_timeout=5,
        ) as ws:
            await ws.send(json.dumps({"type": "auth", "token": BROKER_SECRET}))
            await ws.send(json.dumps({"type": "resize", "rows": 24, "cols": 80}))
            # Let the relay pump the frames (and the PTY's output echo).
            await asyncio.sleep(1.5)

    asyncio.run(drive_frames())

    # 3. The annotation must have advanced — the only writer since the claim
    #    is the relay's frame touch.
    deadline = time.monotonic() + 10
    touched = base
    while time.monotonic() < deadline and touched <= base:
        time.sleep(1)
        touched = _last_used(sandbox)
    assert touched > base, (
        f"broker-last-used not refreshed by WS traffic ({base} → {touched}); "
        "an actively-used terminal would be parked mid-session (issue #158)"
    )
