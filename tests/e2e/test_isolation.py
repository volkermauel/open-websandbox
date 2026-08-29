# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Multi-tenant isolation e2e tests (negative tests) for the sandbox broker.

Runs against the same live broker as ``test_smoke`` but exercises the
multi-tenant isolation guarantees a shared deployment must uphold:

  1. Cross-tenant file denial — tenant B cannot read a file tenant A wrote.
  2. Path-traversal rejection — ``/files/read`` rejects absolute (``/etc/passwd``)
     and ``..`` paths that would escape the per-session workspace.
  3. Peer-pod :8888 denial — a sandbox cannot reach *another* sandbox pod's
     :8888 (NetworkPolicy default-deny ingress + RFC1918 egress block).
  4. Pod-env isolation — the sandbox pod's environment exposes neither the per-session
     runtime key (delivered as a file since PR #50, NOT an env var) nor any
     ``<SVC>_SERVICE_HOST/_PORT`` service-link var (``enableServiceLinks=false``).
     Asserted by exec-ing into the runtime container.

All tests skip automatically when the broker isn't reachable (no live cluster):
each test requests the ``require_broker`` fixture FIRST, so it skips fast
instead of burning the sandbox-claim timeout.
"""
import base64
import json
import os
import re
import subprocess
import uuid

import pytest

# Namespace the sandbox pods + per-session key Secrets live in (workflow RUNTIME_NS).
RUNTIME_NS = os.environ.get("RUNTIME_NS", "agent-sandbox-runtime")
_KUBECTL_TIMEOUT = 30


def _kubectl(*args: str) -> str:
    """Run kubectl, returning stdout; fail the test (not raise) on a non-zero exit.

    Uses a list (no shell) so pod/secret names pass through unquoted and safely."""
    proc = subprocess.run(
        ["kubectl", *args], capture_output=True, text=True, timeout=_KUBECTL_TIMEOUT,
    )
    if proc.returncode != 0:
        pytest.fail(
            f"kubectl {' '.join(args)} failed (exit {proc.returncode}): {proc.stderr.strip()[:500]}"
        )
    return proc.stdout


def _sandbox_for_session(session_id: str) -> str:
    """The per-session Sandbox CR name for a broker session.

    The broker annotates every per-session Sandbox it creates with
    ``broker-session=<session_id>`` (broker/main.py), so this maps a claimed session to
    its Sandbox CR independent of profile (ephemeral vs persistent) or the naming hash."""
    items = json.loads(_kubectl("get", "sandbox", "-n", RUNTIME_NS, "-o", "json")).get("items", [])
    for it in items:
        ann = it.get("metadata", {}).get("annotations") or {}
        if ann.get("broker-session") == session_id:
            return it["metadata"]["name"]
    pytest.fail(f"no Sandbox CR annotated broker-session={session_id!r} in {RUNTIME_NS}")


def _pod_for_sandbox(sandbox_name: str) -> str:
    """The pod backing a Sandbox CR.

    Primary: the pod whose ownerReference is the Sandbox (controller-set, enables GC).
    Fallback: the agent-sandbox controller names the pod after the Sandbox object."""
    items = json.loads(_kubectl("get", "pod", "-n", RUNTIME_NS, "-o", "json")).get("items", [])
    for p in items:
        for ref in p.get("metadata", {}).get("ownerReferences") or []:
            if ref.get("kind") == "Sandbox" and ref.get("name") == sandbox_name:
                return p["metadata"]["name"]
    if any(p["metadata"]["name"] == sandbox_name for p in items):
        return sandbox_name
    pytest.fail(f"no pod owned by Sandbox {sandbox_name!r} in {RUNTIME_NS}")


def _pod_env(pod_name: str) -> dict[str, str]:
    """The runtime container's environment — exactly what untrusted code sees via env/os.environ.

    ``env`` (coreutils) prints the container's configured env: the same set the kubelet
    hands every process, including pid 1. It is the precise attack surface and is reliable
    across runc + gVisor (no ``/proc/1/environ`` ptrace/YAMA dependency)."""
    out = _kubectl("exec", pod_name, "-n", RUNTIME_NS, "-c", "runtime", "--", "env")
    env: dict[str, str] = {}
    for line in out.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            env[key] = value
    return env


def _runtime_key_value(sandbox_name: str) -> str | None:
    """The per-session runtime API key value (file-based, PR #50) minted for this sandbox.

    Reads Secret ``owui-runtime-key-<sandbox>`` (data key ``api-key``). None when the
    Secret is absent — but the runtime could not have booted without it, so the caller
    treats None as a hard failure."""
    proc = subprocess.run(
        ["kubectl", "get", "secret", f"owui-runtime-key-{sandbox_name}", "-n", RUNTIME_NS,
         "-o", "jsonpath={.data.api-key}"],
        capture_output=True, text=True, timeout=_KUBECTL_TIMEOUT,
    )
    raw = proc.stdout.strip()
    if proc.returncode != 0 or not raw:
        return None
    return base64.b64decode(raw).decode("utf-8", "replace")



def test_cross_tenant_file_denial(require_broker, broker, second_broker):
    """Tenant B must NOT observe a file written by tenant A (same path, own session)."""
    name = f"secret-{uuid.uuid4().hex[:8]}.txt"
    secret = f"A-ONLY-{uuid.uuid4().hex[:12]}"

    wrote = broker.post("/files/write", json={"path": name, "content": secret})
    assert wrote.status_code == 200, wrote.text

    # Tenant B reads the SAME path under its OWN (user, session) — different sandbox.
    read = second_broker.get("/files/read", params={"path": name})

    # Core guarantee: tenant B never observes tenant A's secret bytes.
    assert secret not in read.text, (
        f"CROSS-TENANT LEAK: tenant B read tenant A's file "
        f"(HTTP {read.status_code}): {read.text[:200]}"
    )
    # Acceptable dispositions: explicitly denied (403), not found in B's workspace
    # (404), or B's own empty workspace (200). A 5xx would be a bug.
    allowed_as_b = (200, 403, 404)
    assert read.status_code in allowed_as_b, (
        f"unexpected status reading A's file as B: {read.status_code} {read.text[:200]}"
    )


def test_files_read_rejects_path_traversal(require_broker, broker):
    """``/files/read`` must reject absolute and ``..`` paths escaping the workspace."""
    bad_paths = [
        "/etc/passwd",                 # absolute host path
        "../../../../etc/passwd",      # `..` traversal above the workspace root
    ]
    rejected = (400, 403, 404)
    for bad in bad_paths:
        resp = broker.get("/files/read", params={"path": bad})
        assert resp.status_code in rejected, (
            f"path {bad!r} was not rejected: {resp.status_code} {resp.text[:200]}"
        )
        assert "root:" not in resp.text, (
            f"path {bad!r} leaked /etc/passwd contents: {resp.text[:200]}"
        )


def test_peer_pod_8888_denied(require_broker, broker, second_broker):
    """A sandbox must NOT be able to reach another sandbox pod on :8888.

    Discovers tenant B's sandbox pod IP from inside B's own sandbox, then from
    inside tenant A's sandbox tries hard (python socket / curl / wget) to open
    ``http://<B-pod-ip>:8888``. The NetworkPolicy (ingress only from the system
    namespace on 8888 + egress blocking all RFC1918) must make every attempt fail.
    """
    # Resolve tenant B's sandbox pod IP from inside tenant B's sandbox.
    probe = second_broker.post(
        "/execute",
        json={"command": "hostname -I | tr ' ' '\\n' | grep -E '^[0-9.]+$' | head -n1"},
    )
    assert probe.status_code == 200, probe.text
    assert probe.json().get("exit_code") == 0, probe.json()
    peer_ip = probe.json()["stdout"].strip()
    assert re.fullmatch(r"\d{1,3}(?:\.\d{1,3}){3}", peer_ip), (
        f"could not determine a valid IPv4 peer pod IP: {peer_ip!r}"
    )

    # From inside tenant A's sandbox, attempt every likely tool to reach B:8888.
    # If ANY connects, isolation is broken. A heredoc keeps the python probe portable.
    cmd = (
        "connected=0\n"
        "python3 - <<'PY' 2>/dev/null && connected=1\n"
        "import socket, sys\n"
        "try:\n"
        f"    s = socket.create_connection((\"{peer_ip}\", 8888), 5)\n"
        "    s.close()\n"
        "    sys.exit(0)\n"
        "except Exception:\n"
        "    sys.exit(1)\n"
        "PY\n"
        f"curl -sS --max-time 8 -o /dev/null 'http://{peer_ip}:8888/' 2>/dev/null && connected=1\n"
        # --tries=1: GNU wget defaults to 20 retries; the netpol silently drops
        # SYNs, so 20 x the -T timeout would hang the /execute request.
        f"wget -q -T 8 --tries=1 -O /dev/null 'http://{peer_ip}:8888/' 2>/dev/null && connected=1\n"
        'test "$connected" = 1 && echo SANDBOX_CONNECTED_TO_PEER || echo PEER_8888_BLOCKED'
    )
    ran = broker.post("/execute", json={"command": cmd})
    assert ran.status_code == 200, ran.text
    body = ran.json()
    assert body.get("exit_code") == 0, body
    out = body.get("stdout", "")

    assert "SANDBOX_CONNECTED_TO_PEER" not in out, (
        f"ISOLATION BROKEN: sandbox reached peer pod :8888 ({peer_ip})\n{out}"
    )
    assert "PEER_8888_BLOCKED" in out, (
        f"peer-pod :8888 probe did not complete as expected: {out!r}"
    )


def test_pod_env_has_no_runtime_key_secret(require_broker, ready_session):
    """The per-session runtime key is file-based (projected Secret -> /etc/runtime-key/api-key);
    its value MUST NOT appear anywhere in the sandbox pod's environment (issue #48, Assert A).

    Untrusted code in the sandbox can read the pod ENV (/proc/1/environ, env, os.environ),
    so the broker<->runtime credential — which authorizes /execute, /files/*, terminals —
    must never be an env var. PR #50 made it a file; this test proves that invariant holds
    at runtime by reading the live pod env and comparing it to the Secret's value."""
    sandbox = _sandbox_for_session(ready_session)
    pod = _pod_for_sandbox(sandbox)
    env = _pod_env(pod)

    # A.1 — the configured key value is absent from every env var value.
    key = _runtime_key_value(sandbox)
    assert key, (
        f"per-session key Secret owui-runtime-key-{sandbox} missing; "
        "the runtime could not have booted without it"
    )
    leaked = {name: val for name, val in env.items() if key in val}
    assert not leaked, f"per-session runtime key leaked into pod env: {list(leaked)}"

    # A.2 — no env var named like a credential carries a non-empty value.
    sensitive = re.compile(r"(KEY|SECRET|TOKEN|PASSWORD)", re.IGNORECASE)
    named = {name: val for name, val in env.items() if sensitive.search(name) and val}
    assert not named, f"sensitive-looking env vars are non-empty: {named}"

    # A.3 — the legacy env-based name is not set at all.
    assert "RUNTIME_API_KEY" not in env, "RUNTIME_API_KEY must not be set (key is file-based)"


def test_pod_env_has_no_service_links(require_broker, ready_session):
    """``enableServiceLinks=false`` must suppress every ``<SVC>_SERVICE_HOST/_PORT`` env var
    in the sandbox pod (issue #48, Assert B) so k8s service topology does not leak to
    untrusted code. The kubelet-injected ``KUBERNETES_SERVICE_HOST/_PORT`` are the only
    allowed exception (not controlled by enableServiceLinks; scrubbed to '' by the template)."""
    sandbox = _sandbox_for_session(ready_session)
    pod = _pod_for_sandbox(sandbox)
    env = _pod_env(pod)

    allowed = {"KUBERNETES_SERVICE_HOST", "KUBERNETES_SERVICE_PORT"}
    service_link = re.compile(r"^[A-Z0-9_]+_SERVICE_(HOST|PORT)$")
    found = {name for name in env if service_link.match(name) and name not in allowed}
    assert not found, (
        f"service-link env vars present despite enableServiceLinks=false "
        f"(topology leak to untrusted code): {sorted(found)}"
    )
