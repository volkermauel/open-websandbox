# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""S3-tiered end-to-end against a live broker + in-cluster MinIO (issue #52).

Opt-in: skipped unless E2E_S3=1 (the default runc/gvisor matrix is unaffected). Exercises
the REAL data movement + per-session isolation:

  1. Offload-on-reap — write files into a pod /workspace, let the reaper offload, assert
     the data landed in MinIO as users/<uid>/chats/<sid>/workspace-<ts>.tar.zst.
  2. Restore-on-resume — after offload + reap (pod deleted), resume the SAME session into a
     NEW pod and assert the files are restored (synchronous, D4).
  3. Cross-session isolation — two users/chats reap+resume; each resume sees ONLY its own
     data (per-session object keying users/<uid>/chats/<sid>/; /restore pulls one object).

Reuses the broker httpx + claim helpers from conftest; MinIO is inspected by exec'ing the
broker's own boto3 + projected creds (conftest.minio_list_objects).
"""
from __future__ import annotations

import hashlib
import re
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

RUNTIME_NS = "agent-sandbox-runtime"
S3_KEY_RE = re.compile(r"^users/[^/]+/chats/[^/]+/workspace-\d{10}\.tar\.zst$")
OFFLOAD_TIMEOUT = 240  # IDLE_TTL(15) + reaper poll(10) + generous CI slack


def _sbx_name(user: str, session: str) -> str:
    """Mirror broker._chat_sandbox_name so the test can poll the CR for reap."""
    return "owui-c-" + hashlib.sha256(f"{user}/{session}".encode()).hexdigest()[:12]


def _kubectl(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(["kubectl", *args], capture_output=True, text=True, timeout=60)


def _write(c: httpx.Client, user: str, session: str, path: str, marker: str) -> None:
    r = c.post("/execute", json={"command": f"echo {marker} > {path}"},
               headers=headers_for(user, session), timeout=CLAIM_TIMEOUT)
    assert r.status_code == 200 and r.json().get("exit_code") == 0, r.text


def _read(c: httpx.Client, user: str, session: str, path: str) -> tuple[int, str]:
    r = c.post("/execute", json={"command": f"cat {path}"},
               headers=headers_for(user, session), timeout=CLAIM_TIMEOUT)
    body = r.json() if r.status_code == 200 else {}
    return body.get("exit_code", -1), body.get("stdout", "")


def _wait_offloaded(timeout: int = OFFLOAD_TIMEOUT) -> list[str]:
    """Poll MinIO until >=1 workspace-*.tar.zst object appears; return all object keys."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        objs = [o for o in minio_list_objects("users/") if o.endswith(".tar.zst")]
        if objs:
            return objs
        time.sleep(5)
    pytest.fail(f"no S3 offload object appeared within {timeout}s")


def _wait_reaped(user: str, session: str, timeout: int = OFFLOAD_TIMEOUT) -> None:
    """Poll the Sandbox CR until the reaper has deleted it (proves a fresh pod on resume)."""
    name = _sbx_name(user, session)
    deadline = time.time() + timeout
    while time.time() < deadline:
        r = _kubectl(["-n", RUNTIME_NS, "get", "sandbox", name, "--ignore-not-found"])
        if r.stdout.strip() == "":  # CR gone -> reaped
            return
        time.sleep(5)
    pytest.fail(f"sandbox {name} was not reaped within {timeout}s")


# --- 1. offload-on-reap ---------------------------------------------------------
def test_s3_offload_on_reap(require_s3):
    user, session = "u-offload", f"off-{uuid.uuid4().hex[:6]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim_ready_session(c, user, session)
        marker = f"OFFLOAD-{uuid.uuid4().hex[:8]}"
        _write(c, user, session, "/workspace/a.txt", marker)  # last touch -> idle timer starts

        objs = _wait_offloaded()
    assert objs, "no offload object landed in MinIO"
    # D3 object format: users/<uid>/chats/<sid>/workspace-<ts>.tar.zst (versioned, atomic)
    assert all(S3_KEY_RE.match(o) for o in objs), objs


# --- 2. restore-on-resume (synchronous, D4) -------------------------------------
def test_s3_restore_on_resume(require_s3):
    user, session = "u-restore", f"res-{uuid.uuid4().hex[:6]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        _claim_ready_session(c, user, session)
        marker = f"RESTORE-{uuid.uuid4().hex[:8]}"
        _write(c, user, session, "/workspace/r.txt", marker)

        _wait_offloaded()              # data is durably in MinIO
        _wait_reaped(user, session)    # the old pod is gone -> the next resolve MUST restore

        # resume: resolve_sandbox creates a NEW emptyDir pod + synchronously restores first
        _claim_ready_session(c, user, session)
        ec, out = _read(c, user, session, "/workspace/r.txt")
    assert ec == 0, f"restore did not bring r.txt back (exit={ec})"
    assert marker in out, f"restored content mismatch: {out!r}"


# --- 3. cross-session isolation (no leak) ---------------------------------------
def test_s3_cross_session_isolation(require_s3):
    # (a) different user + different chat, and (b) same user, different chat.
    a = ("u-iso-a", f"iso-a-{uuid.uuid4().hex[:6]}")     # writes a.txt
    b = ("u-iso-b", f"iso-b-{uuid.uuid4().hex[:6]}")     # writes b.txt (other user/chat)
    c_sess = ("u-iso-a", f"iso-c-{uuid.uuid4().hex[:6]}")  # writes c.txt (same user as A)
    marker_a, marker_b, marker_c = f"A-{uuid.uuid4().hex[:6]}", f"B-{uuid.uuid4().hex[:6]}", f"C-{uuid.uuid4().hex[:6]}"

    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as probe:
        # Phase 1 — offload each session ONE AT A TIME so the kind node never runs
        # more than one sandbox pod (a constrained KIND node cannot hold 3 concurrent
        # warm+s3 pods; the default e2e proves the node holds ~2).
        for (u, s), fn, mk in [(a, "a.txt", marker_a), (b, "b.txt", marker_b), (c_sess, "c.txt", marker_c)]:
            _claim_ready_session(probe, u, s)
            _write(probe, u, s, f"/workspace/{fn}", mk)
            _wait_offloaded()
            _wait_reaped(u, s)           # pod gone before creating the next

        # Phase 2 — resume each session ONE AT A TIME and assert it sees ONLY its own
        # data (reaping between resumes to keep one restored pod on the node at a time).

        # resume A -> only a.txt; b.txt + c.txt must be ABSENT (no cross-session leak)
        _claim_ready_session(probe, *a)
        ec_a, out_a = _read(probe, *a, "/workspace/a.txt")
        ec_b, _ = _read(probe, *a, "/workspace/b.txt")
        ec_c, _ = _read(probe, *a, "/workspace/c.txt")
        assert ec_a == 0 and marker_a in out_a
        assert ec_b != 0, "session A can see session B's data (leak!)"
        assert ec_c != 0, "session A can see session C's data (same-user cross-chat leak!)"
        _wait_offloaded()
        _wait_reaped(*a)

        # resume B -> only b.txt
        _claim_ready_session(probe, *b)
        ec_b2, out_b2 = _read(probe, *b, "/workspace/b.txt")
        ec_a2, _ = _read(probe, *b, "/workspace/a.txt")
        assert ec_b2 == 0 and marker_b in out_b2
        assert ec_a2 != 0, "session B can see session A's data (leak!)"
        _wait_offloaded()
        _wait_reaped(*b)

        # resume C -> only c.txt (same user as A, but different chat -> isolated)
        _claim_ready_session(probe, *c_sess)
        ec_c2, out_c2 = _read(probe, *c_sess, "/workspace/c.txt")
        ec_a3, _ = _read(probe, *c_sess, "/workspace/a.txt")
        assert ec_c2 == 0 and marker_c in out_c2
        assert ec_a3 != 0, "session C can see session A's data (same-user cross-chat leak!)"
