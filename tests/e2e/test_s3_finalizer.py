# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Termination-ordered S3 snapshot (openspec s3-termination-finalizer).

Opt-in with the s3 lane (E2E_S3=1). Proves the goal-mode data contract:

  1. sha256 round-trip — write bulk random data, hash it in the pod, let the
     reaper idle-reap the sandbox (removed from agent-sandbox-runtime), resume
     into a fresh pod, hash again, assert the hashes are identical.
  2. Finalizer holds deletion — an out-of-band `kubectl delete sandbox`
     (not broker-initiated) leaves the CR Terminating with the
     owui.dev/s3-snapshot finalizer while the broker offloads; only then the
     object goes away, and a later resolve restores the data.

Reuses the claim/execute helpers + MinIO access from conftest.
"""
from __future__ import annotations

import subprocess
import time
import uuid

import httpx
from conftest import (  # type: ignore[import-not-found]
    BROKER_URL,
    CLAIM_TIMEOUT,
    _claim_ready_session,
    headers_for,
)
from test_s3_tiered import (  # type: ignore[import-not-found]
    OFFLOAD_TIMEOUT,
    RUNTIME_NS,
    SYS_NS,
    _sbx_name,
)

FINALIZER = "owui.dev/s3-snapshot"
BLOB_BYTES = 8 * 1024 * 1024  # 8 MiB random — CI-fast, still a real archive


def _kubectl(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(["kubectl", *args], capture_output=True, text=True, timeout=60)


def _exec(c: httpx.Client, user: str, session: str, cmd: str) -> tuple[int, str]:
    r = c.post("/execute", json={"command": cmd, "timeout": 120},
               headers=headers_for(user, session), timeout=CLAIM_TIMEOUT)
    body = r.json() if r.status_code == 200 else {}
    return body.get("exit_code", -1), body.get("stdout", "")


def _wait_sandbox_gone(name: str, timeout: int = OFFLOAD_TIMEOUT) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        r = _kubectl(["-n", RUNTIME_NS, "get", "sandbox", name, "--ignore-not-found"])
        if r.stdout.strip() == "":
            return
        time.sleep(2)
    pytest_fail_gone(name)


def pytest_fail_gone(name: str) -> None:  # pragma: no cover - diagnostics helper
    r = _kubectl(["-n", RUNTIME_NS, "get", "sandbox", name, "-o", "yaml", "--ignore-not-found"])
    print(f"\n=== DIAG: sandbox {name} stuck ===\n{r.stdout}", flush=True)
    logs = _kubectl(["-n", SYS_NS, "logs", "deploy/owui-broker", "--tail=80"])
    print(logs.stdout, flush=True)
    raise AssertionError(f"sandbox {name} was not removed within timeout")


# --- 1. sha256 round-trip: write -> idle-reap -> restore -> hash again ----------
def test_s3_sha256_roundtrip(require_s3):
    user, session = "u-sha", f"sha-{uuid.uuid4().hex[:6]}"
    name = _sbx_name(user, session)
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim_ready_session(c, user, session)
        ec, pre = _exec(
            c, user, session,
            f"head -c {BLOB_BYTES} /dev/urandom > /workspace/blob.bin"
            f" && sha256sum /workspace/blob.bin",
        )
        assert ec == 0 and len(pre.strip()) == 64, pre

        # idle-reap: the sandbox must vanish from the runtime namespace
        _wait_sandbox_gone(name, timeout=OFFLOAD_TIMEOUT)

        # resume: fresh pod, synchronous restore from S3 before readiness
        _claim_ready_session(c, user, session)
        ec, post = _exec(c, user, session, "sha256sum /workspace/blob.bin")
        assert ec == 0 and len(post.strip()) == 64, post
        assert pre.split()[0] == post.split()[0], f"sha256 mismatch: {pre!r} vs {post!r}"


# --- 2. finalizer: out-of-band delete archives before the pod dies --------------
def test_s3_finalizer_holds_delete(require_s3):
    user, session = "u-fin", f"fin-{uuid.uuid4().hex[:6]}"
    name = _sbx_name(user, session)
    marker = f"FINALIZER-{uuid.uuid4().hex[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim_ready_session(c, user, session)
        ec, _ = _exec(c, user, session, f"echo {marker} > /workspace/finalizer.txt")
        assert ec == 0

        # out-of-band delete with NO fresh offload yet -> broker must archive
        # while the CR is Terminating, then drop the finalizer.
        r = _kubectl(["-n", RUNTIME_NS, "delete", "sandbox", name, "--wait=false"])
        assert r.returncode == 0, r.stderr

        saw_terminating = False
        deadline = time.time() + OFFLOAD_TIMEOUT
        while time.time() < deadline:
            cur = _kubectl(["-n", RUNTIME_NS, "get", "sandbox", name, "--ignore-not-found"])
            if cur.stdout.strip() == "":
                break
            js = _kubectl(["-n", RUNTIME_NS, "get", "sandbox", name, "-o",
                           "jsonpath={.metadata.deletionTimestamp}"
                           "{'|'}{.metadata.finalizers}"])
            if "owui.dev/s3-snapshot" in js.stdout and js.stdout.startswith("20"):
                saw_terminating = True
            time.sleep(1)
        _wait_sandbox_gone(name)

        # deletion completed -> resume must restore the un-archived-at-idle data
        _claim_ready_session(c, user, session)
        ec, out = _exec(c, user, session, "cat /workspace/finalizer.txt")
        assert ec == 0 and marker in out, f"finalizer-path data lost: {out!r}"
        assert saw_terminating, "sandbox was deleted without ever showing the finalizer"
