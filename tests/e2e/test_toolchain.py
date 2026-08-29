# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Workbench toolchain e2e (openspec/changes/2026-08-29-workbench-toolchain).

Two halves:

* ``test_gen_manifest_self_test`` — the tools.json → manifest generator's own
  schema/rendering invariants, run LOCALLY (no broker, no cluster needed).
* the rest — the baked toolchain surface through the broker relay, exactly as
  an Open Web UI chat would see it: the ``/system`` capability append,
  ``sandbox-tools`` live state, the sudo-apt-only whitelist (figlet in, ``rm``
  denied), and the PEP-668-relieved ``pip install --target`` recipe.
"""

import hashlib
import os
import subprocess
from pathlib import Path

import httpx
import pytest
from conftest import (  # noqa: E402
    BROKER_URL,
    TEST_USER,
    _claim_ready_session,
    headers_for,
)

needs_broker = pytest.mark.skipif(
    not os.getenv("BROKER_URL") or not os.getenv("BROKER_SECRET"),
    reason="needs a live broker (KIND lane exports BROKER_URL + BROKER_SECRET)",
)

CLAIM_TIMEOUT = 300.0

REPO_ROOT = Path(__file__).resolve().parents[2]
GEN_MANIFEST = REPO_ROOT / "rust" / "runtime" / "gen-manifest.py"
TOOLS_JSON = REPO_ROOT / "rust" / "runtime" / "tools.json"


def test_gen_manifest_self_test():
    """gen-manifest.py --self-test validates tools.json + rendering invariants.

    Runs everywhere (no broker): schema, path-free manifest, token budget.
    """
    r = subprocess.run(
        ["python3", str(GEN_MANIFEST), "--tools", str(TOOLS_JSON), "--self-test"],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert r.returncode == 0, r.stdout + r.stderr
    assert "PASS" in r.stdout


@needs_broker
def test_system_appends_toolchain_manifest_and_conventions():
    """GET /system keeps the upstream prompt and appends the toolchain sections."""
    session = f"tc-sys-{hashlib.sha256(b'tc-sys').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        resp = client.get("/system", headers=headers_for(TEST_USER, session))
        assert resp.status_code == 200, resp.text[:200]
        prompt = resp.json()["prompt"]

        # Append-only: the upstream-verbatim tail is still the tail.
        assert prompt.endswith(
            "If a command produces no output, that typically means it succeeded."
        )

        # The two knob-gated sections (SANDBOX_TOOLS_MANIFEST, default-on).
        assert "## Available toolchain (base image)" in prompt
        assert "## Workspace conventions" in prompt
        # A probed pip pin (build-time inventory line) and the WORKDIR-driven
        # scratch convention (rendered from the configured workspace root).
        assert "pandas" in prompt
        assert "/workspace/tmp" in prompt


@needs_broker
def test_sandbox_tools_reports_manifest_and_live_state():
    """`sandbox-tools` prints the manifest, a live delta, and exits 0."""
    session = f"tc-tools-{hashlib.sha256(b'tc-tools').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        r = client.post(
            "/execute",
            json={"command": "sandbox-tools | head -60"},
            headers=headers_for(TEST_USER, session),
        )
        assert r.status_code == 200, r.text[:200]
        body = r.json()
        assert body["exit_code"] == 0, body
        assert "Toolchain baked into this base image" in body["stdout"], body
        assert "## Live state (probed now)" in body["stdout"], body


@needs_broker
def test_sudo_apt_whitelist_allows_installs_denies_everything_else():
    """sudo is apt-get-verbs-only: figlet installs, `sudo rm` is denied."""
    session = f"tc-sudo-{hashlib.sha256(b'tc-sudo').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        hdrs = headers_for(TEST_USER, session)

        # Allowed verb: apt-get install (writes the ephemeral rootfs).
        up = client.post(
            "/execute",
            json={"command": "sudo apt-get update -qq && sudo apt-get install -y figlet"},
            headers=hdrs,
        )
        assert up.status_code == 200, up.text[:200]
        assert up.json()["exit_code"] == 0, up.json()

        run = client.post(
            "/execute", json={"command": "figlet hi"}, headers=hdrs
        )
        assert run.status_code == 200, run.text[:200]
        body = run.json()
        assert body["exit_code"] == 0, body
        assert "_" in body["stdout"], body  # figlet banner, not empty output

        # Denied verb: nothing but apt-get verbs is whitelisted. No NOPASSWD
        # rule matches `rm`, so sudo demands a password the locked sandbox
        # account does not have — non-interactive /execute has no tty either.
        denied = client.post(
            "/execute", json={"command": "sudo rm /tmp/nope"}, headers=hdrs
        )
        assert denied.status_code == 200, denied.text[:200]
        dbody = denied.json()
        assert dbody["exit_code"] != 0, dbody
        assert "password is required" in dbody["stderr"], dbody
        # ... and the file was never touched by privilege.
        probe = client.post(
            "/execute", json={"command": "ls /tmp/nope 2>&1; true"}, headers=hdrs
        )
        assert "No such file or directory" in probe.json()["stdout"], probe.json()


@needs_broker
def test_pip_target_install_and_pythonpath_import():
    """PEP-668 relief: plain `pip install --target` + PYTHONPATH import works."""
    session = f"tc-pip-{hashlib.sha256(b'tc-pip').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        hdrs = headers_for(TEST_USER, session)

        # No --break-system-packages needed: EXTERNALLY-MANAGED is removed in
        # the image. `six` is a tiny pure-Python wheel (PyPI egress is allowed).
        up = client.post(
            "/execute",
            json={"command": "pip install --no-cache-dir --target /packages/py six"},
            headers=hdrs,
        )
        assert up.status_code == 200, up.text[:200]
        assert up.json()["exit_code"] == 0, up.json()

        run = client.post(
            "/execute",
            json={
                "command": "PYTHONPATH=/packages/py python3 -c "
                "'import six; print(\"six-ok\", six.__version__)'"
            },
            headers=hdrs,
        )
        assert run.status_code == 200, run.text[:200]
        body = run.json()
        assert body["exit_code"] == 0, body
        assert "six-ok" in body["stdout"], body
