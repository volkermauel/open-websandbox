"""Multi-tenant isolation e2e tests (negative tests) for the sandbox broker.

Runs against the same live broker as ``test_smoke`` but exercises the
multi-tenant isolation guarantees a shared deployment must uphold:

  1. Cross-tenant file denial — tenant B cannot read a file tenant A wrote.
  2. Path-traversal rejection — ``/files/read`` rejects absolute (``/etc/passwd``)
     and ``..`` paths that would escape the per-session workspace.
  3. Peer-pod :8888 denial — a sandbox cannot reach *another* sandbox pod's
     :8888 (NetworkPolicy default-deny ingress + RFC1918 egress block).

All three skip automatically when the broker isn't reachable (no live cluster):
each test requests the ``require_broker`` fixture FIRST, so it skips fast
instead of burning the sandbox-claim timeout.
"""
import re
import uuid



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
        f"wget -q -T 8 -O /dev/null 'http://{peer_ip}:8888/' 2>/dev/null && connected=1\n"
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
