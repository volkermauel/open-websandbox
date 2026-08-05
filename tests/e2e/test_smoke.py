"""Smoke + correctness tests for the open-sandbox broker→router→runtime path.

Run against a live broker (KIND/runc via the Helm chart). The sandbox is claimed once
in conftest (`ready_session`) and reused by every test through the `broker` client.
"""
import uuid


def test_broker_healthz(broker):
    """Broker is up and serving its liveness endpoint."""
    r = broker.get("/healthz")
    assert r.status_code == 200, r.text


def test_execute_echo(broker):
    """POST /execute runs a command and returns captured stdout + exit code."""
    marker = f"hello-e2e-{uuid.uuid4().hex[:6]}"
    r = broker.post("/execute", json={"command": f"echo {marker}"})
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["exit_code"] == 0, body
    assert marker in body["stdout"], body


def test_execute_exit_code(broker):
    """A failing command surfaces a non-zero exit code (not an HTTP error)."""
    r = broker.post("/execute", json={"command": "exit 7"})
    assert r.status_code == 200, r.text
    assert r.json()["exit_code"] == 7, r.json()


def test_files_write_then_read(broker):
    """POST /files/write then GET /files/read round-trips content in the workspace."""
    name = f"e2e-file-{uuid.uuid4().hex[:6]}.txt"
    payload = f"file-content-{uuid.uuid4().hex[:6]}"
    w = broker.post("/files/write", json={"path": name, "content": payload})
    assert w.status_code == 200, w.text
    r = broker.get("/files/read", params={"path": name})
    assert r.status_code == 200, r.text
    assert payload in r.json()["content"], r.json()


def test_workspace_persists_across_requests(broker):
    """Within one session the workspace persists across separate requests."""
    token = f"persist-{uuid.uuid4().hex[:8]}"
    w = broker.post("/execute", json={"command": f"echo {token} > /workspace/persist.txt"})
    assert w.status_code == 200 and w.json()["exit_code"] == 0, w.json()
    r = broker.post("/execute", json={"command": "cat /workspace/persist.txt"})
    assert r.status_code == 200, r.text
    assert token in r.json()["stdout"], r.json()
