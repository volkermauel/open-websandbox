# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Hybrid PVC × S3 tiering e2e (issue #142) — tiering independent of the hot tier.

Opt-in (``E2E_PVC_S3=1``): runs against a chart install with
``broker.persistentMode: per-user-pvc`` AND ``broker.s3.enabled: true``
(values-kind-pvc-s3.yaml, in-cluster MinIO fixture). Proves the three hybrid
behaviours that only exist when the cold tier composes with a PVC hot tier:

  1. **Park-resume is a hot hit** — after park (pod deleted, PVC retains data),
     resume serves the PVC data and the runtime DECLINES a stale S3 restore
     (restore-if-empty). Newer hot data is never clobbered.
  2. **Reap is a true tier-down** — idle > reapSeconds briefly resumes the
     parked sandbox, offloads /workspace to MinIO, PURGES the chat dir from the
     PVC (verified by a debug pod mounting the PVC), then deletes the sandbox.
  3. **Re-resolve is a cold hit** — the purged chat comes back from S3.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import time
import uuid

import httpx
import pytest
from conftest import (  # type: ignore[import-not-found]
    BROKER_URL,
    CLAIM_TIMEOUT,
    _claim_ready_session,
    headers_for,
    minio_list_objects,
)

RT_NS = os.getenv("E2E_RT_NS", "agent-sandbox-runtime")
# parkIdleSeconds=12, reapSeconds=50, reaperPoll=5 (values-kind-pvc-s3.yaml).
PARK_WAIT = 30          # > parkIdle + controller settle
REAP_WAIT = 150         # > reapSeconds + resume + offload + poll slack

pytestmark = [
    pytest.mark.usefixtures("require_pvc_s3"),
    pytest.mark.skipif(
        os.getenv("E2E_PVC") == "1" and not os.environ.get("E2E_PVC_S3"),
        reason="pvc-s3 lane sets E2E_PVC_S3",
    ),
]


def _run(cmd: list[str], timeout: float = 90) -> subprocess.CompletedProcess[str]:
    """subprocess.run with explicit R1 kubeconfig handling."""
    env = dict(os.environ)
    kc = env.get("KUBECONFIG")
    if kc and cmd[0] == "kubectl" and "--kubeconfig" not in cmd:
        cmd = ["kubectl", "--kubeconfig", kc, *cmd[1:]]
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)


def _sbx_name(user: str, session: str) -> str:
    return "owui-c-" + hashlib.sha256(f"{user}/{session}".encode()).hexdigest()[:12]


def _chat_dir(session: str) -> str:
    return hashlib.sha256(session.encode()).hexdigest()[:12]


def _pvc_name(user: str) -> str:
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


def _sandbox_gone(name: str) -> bool:
    r = _run(["kubectl", "-n", RT_NS, "get", "sandbox", name, "-o", "name"])
    return r.returncode != 0


def _pvc_chat_dir_has_user_data(pvc: str, chat_dir: str) -> bool:
    """Mount the PVC in a debug pod; True if the chat dir holds USER data.

    After reap the chat dir is purged from the hot tier (#142). The runtime's
    SIGTERM scrollback flush (#129) may legitimately re-create
    `.open-websandbox/` inside it as the pod dies — that reserved dir does not
    count. Any OTHER entry means the purge did not run (or left data behind),
    which would also invalidate the cold-restore proof.
    RWO local-path on single-node KIND binds fine once the sandbox pod is gone.
    """
    dbg = f"pvc-dbg-{uuid.uuid4().hex[:6]}"
    r = _run([
        "kubectl", "-n", RT_NS, "run", dbg, "--image=busybox", "--restart=Never",
        "--overrides", (
            '{"spec":{"containers":[{"name":"dbg","image":"busybox",'
            '"command":["sh","-c","ls -A /ws/chats/' + chat_dir + ' 2>/dev/null || true"],'
            '"volumeMounts":[{"name":"ws","mountPath":"/ws"}]}],'
            '"volumes":[{"name":"ws","persistentVolumeClaim":{"claimName":"' + pvc + '"}}]}}'
        ),
    ])
    assert r.returncode == 0, r.stderr
    try:
        _run(["kubectl", "-n", RT_NS, "wait", "--for=condition=Ready", f"pod/{dbg}",
              "--timeout=60s"], timeout=70)
        r = _run(["kubectl", "-n", RT_NS, "exec", dbg, "--", "sh", "-c",
                  f"ls -A /ws/chats/{chat_dir} 2>/dev/null | grep -v -x .open-websandbox || true"],
                 timeout=60)
        return bool((r.stdout or "").strip())
    finally:
        _run(["kubectl", "-n", RT_NS, "delete", "pod", dbg, "--force", "--grace-period=0"])


# --- 1. park-resume serves the PVC (hot hit, no S3 clobber) ----------------------


def test_park_resume_serves_pvc_without_s3_clobber():
    user = f"hy-{uuid.uuid4().hex[:6]}"
    s1 = f"park1-{uuid.uuid4().hex[:6]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        # Write v1 in session A; let it park (pod deleted, PVC retains).
        _claim_ready_session(c, user, s1)
        _exec(c, user, s1, "echo v1 > /workspace/state.txt")
        time.sleep(PARK_WAIT)
        # Resume: PVC must serve v1 (a stale S3 object, if any existed, must
        # NOT win over the hot tier — restore-if-empty).
        code, out = _exec(c, user, s1, "cat /workspace/state.txt")
        assert code == 0 and out.strip() == "v1", out
        # Advance hot data, park again, resume: still the NEWER hot value.
        _exec(c, user, s1, "echo v2 > /workspace/state.txt")
        time.sleep(PARK_WAIT)
        code, out = _exec(c, user, s1, "cat /workspace/state.txt")
        assert code == 0 and out.strip() == "v2", f"hot data regressed: {out!r}"


# --- 2+3. reap offloads + purges; re-resolve restores from the cold tier ---------


def test_reap_offloads_purges_hot_tier_and_restores_on_resolve():
    user = f"hy-{uuid.uuid4().hex[:6]}"
    session = f"reap-{uuid.uuid4().hex[:6]}"
    name = _sbx_name(user, session)
    chat = _chat_dir(session)
    pvc = _pvc_name(user)
    marker = f"COLD-{uuid.uuid4().hex[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim_ready_session(c, user, session)
        _exec(c, user, session, f"echo {marker} > /workspace/cold.txt")

        # Wait past park + reap: the reaper resumes the parked sandbox,
        # offloads to MinIO, purges the chat dir, deletes the sandbox.
        deadline = time.time() + REAP_WAIT
        while time.time() < deadline and not _sandbox_gone(name):
            time.sleep(5)
        assert _sandbox_gone(name), f"sandbox {name} was not reaped within {REAP_WAIT}s"

        # The offload landed in the cold tier (keys use the RAW user/session —
        # mirror of s3_namespace in rust/broker/src/s3.rs).
        keys = minio_list_objects(f"users/{user}/chats/{session}/")
        assert keys, "no S3 object after reap — offload did not run"
        assert any(k.endswith(".tar.zst") for k in keys), keys

        # The chat dir is PURGED of user data (true tier-down, not a copy).
        assert not _pvc_chat_dir_has_user_data(pvc, chat), (
            f"chat dir chats/{chat} still holds user data after reap — purge did not run"
        )

        # Re-resolve: fresh pod, empty subPath → restore-if-empty proceeds and
        # the cold tier serves the marker.
        _claim_ready_session(c, user, session)
        code, out = _exec(c, user, session, "cat /workspace/cold.txt")
        assert code == 0 and marker in out, f"cold restore missing marker: {out!r}"
