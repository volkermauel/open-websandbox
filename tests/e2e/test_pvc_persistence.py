"""PVC hot-tier e2e (issue #140) — per-user-pvc and shared-subpath modes.

Opt-in (``E2E_PVC=1``): runs against a chart install with
``broker.persistentMode`` set to a PVC mode and ``broker.defaultProfile:
persistent`` (see values-kind-pvc.yaml / values-kind-pvc-shared.yaml). The
broker port-forward is the only mandatory fixture; kubectl (R1-safe) drives
sandbox deletion to prove persistence across recreation.

What is proven here:
  1. **Persistence across sandbox recreation** — a marker file written in a chat
     survives deletion of the Sandbox object itself (pod AND object gone): the
     next resolve recreates the sandbox over the same PVC + per-chat subPath.
  2. **Per-chat isolation** — chat B of the SAME user never sees chat A's files
     and cannot delete them (different subPath => the delete "succeeds" but
     operates on B's own directory; A's file is untouched).
  3. **Cross-user isolation** — a different user's chat sees nothing of user 1.
  4. **PVC naming** (per-user-pvc mode) — the broker created
     ``workspace-p-<sha256(user)[:12]>`` with the configured class/mode.

E2E_PVC_MODE selects assertions (``per-user-pvc`` default | ``shared-subpath``).
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import uuid

import httpx
import pytest
from conftest import (
    BROKER_URL,
    CLAIM_TIMEOUT,
    _claim_ready_session,
    headers_for,
)

RT_NS = os.getenv("E2E_RT_NS", "agent-sandbox-runtime")
MODE = os.getenv("E2E_PVC_MODE", "per-user-pvc")

pytestmark = [
    pytest.mark.usefixtures("require_pvc"),
    pytest.mark.skipif(os.getenv("E2E_PVC") != "1", reason="opt-in: set E2E_PVC=1"),
]


def _run(cmd: list[str], timeout: float = 60) -> subprocess.CompletedProcess[str]:
    """subprocess.run with explicit R1 kubeconfig handling."""
    env = dict(os.environ)
    kc = env.get("KUBECONFIG")
    if kc and cmd[0] == "kubectl" and "--kubeconfig" not in cmd:
        cmd = ["kubectl", "--kubeconfig", kc, *cmd[1:]]
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)


def _sandbox_name(user: str, session: str) -> str:
    """Persistent sandbox name (owui-c-<sha256(user/session)[:12]>)."""
    digest = hashlib.sha256(f"{user}/{session}".encode()).hexdigest()[:12]
    return f"owui-c-{digest}"


def _user_pvc_name(user: str) -> str:
    return f"workspace-p-{hashlib.sha256(user.encode()).hexdigest()[:12]}"


def _exec(c: httpx.Client, user: str, session: str, command: str) -> tuple[int, str]:
    r = c.post(
        "/execute",
        json={"command": command},
        headers={**headers_for(user, session), "X-Persistence": "persistent"},
    )
    assert r.status_code == 200, f"{r.status_code}: {r.text[:300]}"
    body = r.json()
    return body.get("exit_code", -1), (body.get("stdout") or "")


def _claim(c: httpx.Client, user: str, session: str) -> None:
    _claim_ready_session(c, user, session)


def _delete_sandbox(name: str) -> None:
    r = _run(["kubectl", "-n", RT_NS, "delete", "sandbox", name, "--wait=true"])
    assert r.returncode == 0, f"delete sandbox {name}: {r.stderr}"


def _pvc_exists(name: str) -> bool:
    r = _run(["kubectl", "-n", RT_NS, "get", "pvc", name, "-o", "name"])
    return r.returncode == 0


# --- 1. persistence across sandbox recreation -----------------------------------


def test_workspace_survives_sandbox_recreation():
    user, session = "u-pvc-recreate", f"rec-{uuid.uuid4().hex[:6]}"
    name = _sandbox_name(user, session)
    marker = f"RECREATE-{uuid.uuid4().hex[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim(c, user, session)
        _exec(c, user, session, f"echo {marker} > /workspace/keep.txt")

        # Kill the Sandbox OBJECT (pod + spec), not just the pod: the next
        # resolve must rebuild it over the same PVC + per-chat subPath.
        _delete_sandbox(name)

        _claim(c, user, session)  # re-resolve recreates the sandbox
        code, out = _exec(c, user, session, "cat /workspace/keep.txt")
        assert code == 0, out
        assert marker in out, f"marker lost after sandbox recreation: {out!r}"


# --- 2. per-chat isolation (the #140 requirement) --------------------------------


def test_chat_b_cannot_see_or_delete_chat_a_files():
    user = "u-pvc-isolation"
    a, b = f"chat-a-{uuid.uuid4().hex[:6]}", f"chat-b-{uuid.uuid4().hex[:6]}"
    secret = f"SECRET-{uuid.uuid4().hex[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim(c, user, a)
        _exec(c, user, a, f"echo {secret} > /workspace/a-secret.txt")

        _claim(c, user, b)
        # Chat B's /workspace is a DIFFERENT subPath: the file is invisible...
        code, out = _exec(c, user, b, "ls /workspace")
        assert code == 0, out
        assert "a-secret" not in out, f"cross-chat LEAK in {MODE}: {out!r}"
        # ...and rm -rf /workspace/* cannot touch chat A's data.
        _exec(c, user, b, "rm -rf /workspace/*")

        code, out = _exec(c, user, a, "cat /workspace/a-secret.txt")
        assert code == 0 and secret in out, (
            f"chat B's rm destroyed chat A's data ({out!r})"
        )


# --- 3. cross-user isolation ------------------------------------------------------


def test_other_user_sees_nothing():
    user1, user2 = "u-pvc-owner", "u-pvc-stranger"
    s1 = f"own-{uuid.uuid4().hex[:6]}"
    s2 = f"str-{uuid.uuid4().hex[:6]}"
    secret = f"MINE-{uuid.uuid4().hex[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim(c, user1, s1)
        _exec(c, user1, s1, f"echo {secret} > /workspace/only-mine.txt")

        _claim(c, user2, s2)
        code, out = _exec(c, user2, s2, "ls /workspace")
        assert code == 0, out
        assert "only-mine" not in out, f"cross-user LEAK in {MODE}: {out!r}"


# --- 4. PVC naming / spec (per-user-pvc mode only) --------------------------------


@pytest.mark.skipif(MODE != "per-user-pvc", reason="per-user PVC is created only in that mode")
def test_broker_created_per_user_pvc():
    # Any prior test claimed TEST_USER? Claim one ourselves to force creation.
    user, session = "u-pvc-naming", f"nam-{uuid.uuid4().hex[:6]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim(c, user, session)
        _exec(c, user, session, "true")

    name = _user_pvc_name(user)
    assert _pvc_exists(name), f"broker did not create {name}"
    r = _run(["kubectl", "-n", RT_NS, "get", "pvc", name, "-o",
              "jsonpath={.spec.accessModes}:{.status.phase}"])
    assert r.returncode == 0, r.stderr
    modes, phase = r.stdout.strip().rsplit(":", 1)
    assert "ReadWriteOnce" in modes or "ReadWriteMany" in modes, modes
    assert phase == "Bound", f"{name} not Bound: {phase}"


# --- 5. both chats of one user mount the SAME per-user PVC (per-user-pvc) --------


@pytest.mark.skipif(MODE != "per-user-pvc", reason="per-user PVC naming applies to that mode")
def test_two_chats_share_one_per_user_pvc():
    user = "u-pvc-share"
    a, b = f"sh-a-{uuid.uuid4().hex[:6]}", f"sh-b-{uuid.uuid4().hex[:6]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim(c, user, a)
        _claim(c, user, b)

    # Exactly one PVC for this user even after two chats resolved.
    prefix = _user_pvc_name(user)
    r = _run(["kubectl", "-n", RT_NS, "get", "pvc", "-o", "name"])
    assert r.returncode == 0, r.stderr
    matching = [ln for ln in r.stdout.splitlines() if prefix in ln]
    assert len(matching) == 1, f"expected exactly one PVC {prefix}, got {matching}"
