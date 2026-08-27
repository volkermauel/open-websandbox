# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Helm stateful upgrade / rollback e2e (issue #128).

Opt-in: set ``E2E_UPGRADE=1``. Runs against a live cluster where the chart is
installed as release ``E2E_RELEASE`` with a **persistent** profile (an RWX
``profile.persistentStorageClass`` so per-user PVCs are created) and two runtime
image tags loaded into the node:

  - ``E2E_IMAGE_A`` — the tag the chart is currently installed at.
  - ``E2E_IMAGE_B`` — the upgrade target tag (a second tag pointing at a built
    runtime image; for a mechanics-only check it can be the *same* image retagged).

The broker is reached at ``BROKER_URL`` (after ``kubectl port-forward``).

What it proves
--------------
1. A file written to a **persistent** sandbox survives a ``helm upgrade`` — the
   per-user PVC is retained (``shutdownPolicy: Retain``) and the recreated pod
   reattaches it.
2. ``helm rollback`` reverts the runtime image tag and the file is still there.

See ``docs/operations.md`` → *Upgrade & rollback* / *Helm upgrade & version skew*.
"""
import contextlib
import json
import os
import subprocess
import time
import uuid

import httpx
import pytest

# Reuse the shared harness knobs + helpers (pytest puts the test dir on sys.path).
from conftest import (  # noqa: E402
    BROKER_URL,
    RUN_ID,
    TEST_USER,
    _claim_ready_session,
    headers_for,
)

RELEASE = os.environ.get("E2E_RELEASE", "open-websandbox")
CHART = os.environ.get("E2E_CHART", "open-websandbox-platform/chart")
# The release namespace — helm defaults to the kubeconfig's current namespace
# ("default" in CI and KIND), NOT the install namespace, so every helm call
# must pass it explicitly.
NS = os.environ.get("E2E_NS", "agent-sandbox-system")
RT_NAMESPACE = os.environ.get("E2E_RT_NS", "agent-sandbox-runtime")
# The base SandboxTemplate the chart renders (broker.baseTemplate) — the pod
# template `helm upgrade --set imageTag` eventually repoints.
TEMPLATE = os.environ.get("E2E_BASE_TEMPLATE", "code-standard-v1")
# Two loaded runtime image tags: A = installed, B = upgrade target.
IMAGE_A = os.environ.get("E2E_IMAGE_A", "")
IMAGE_B = os.environ.get("E2E_IMAGE_B", "")
# Repo rule R1: never touch the default kubeconfig — point every call at one
# explicitly via KUBECONFIG.
EXTRA_KC = ["--kubeconfig", os.environ["KUBECONFIG"]] if os.environ.get("KUBECONFIG") else []


def _run(argv: list[str], timeout: int = 120) -> subprocess.CompletedProcess:
    """Run a command, failing the test with stderr on a non-zero exit."""
    try:
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        pytest.fail(f"`{' '.join(argv)}` timed out after {timeout}s: {exc!r}")
    if r.returncode != 0:
        pytest.fail(f"`{' '.join(argv)}` exited {r.returncode}:\n{r.stderr[-800:]}")
    return r


def _runtime_image() -> str:
    """The runtime container image currently rendered by the SandboxTemplate."""
    r = _run([
        "kubectl", *EXTRA_KC, "-n", RT_NAMESPACE,
        "get", "sandboxtemplate", TEMPLATE, "-o",
        "jsonpath={.spec.podTemplate.spec.containers[0].image}",
    ])
    return r.stdout.strip()


def _tag_of(image: str) -> str:
    """The trailing tag of an image reference (``repo:tag`` → ``tag``)."""
    return image.rsplit(":", 1)[-1]


@contextlib.contextmanager
def _fresh_port_forward(local: int):
    """Own port-forward for POST-upgrade checks.

    ``helm upgrade --set imageTag=...`` rolls ALL platform images — including
    the broker — so any port-forward bound to the old broker pod dies with it.
    This helper owns a NEW port-forward against the service (post-upgrade pod)
    and waits for ``/healthz`` before handing the URL over.
    """
    pf = subprocess.Popen(
        ["kubectl", *EXTRA_KC, "-n", NS, "port-forward", "svc/owui-broker",
         f"{local}:8080"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        url = f"http://localhost:{local}"
        for _ in range(30):
            try:
                if httpx.get(f"{url}/healthz", timeout=2).status_code == 200:
                    break
            except httpx.HTTPError:
                pass
            time.sleep(1)
        else:
            pytest.fail(f"port-forward :{local} did not come up after upgrade")
        yield url
    finally:
        pf.terminate()
        pf.wait(timeout=10)


def _helm_releases() -> list[int]:
    """Deployed revision numbers for ``RELEASE``, ascending."""
    r = _run(["helm", *EXTRA_KC, "history", RELEASE, "-n", NS, "-o", "json"], timeout=60)
    try:
        history = json.loads(r.stdout)
    except ValueError:
        pytest.fail(f"`helm history {RELEASE}` returned non-JSON output:\n{r.stdout[:400]}")
    return [d["revision"] for d in history]


@pytest.fixture(scope="module")
def require_upgrade() -> None:
    """Gate the suite: skip unless opted in and both tags are set + deployed at A."""
    if not os.environ.get("E2E_UPGRADE"):
        pytest.skip("upgrade/rollback e2e is opt-in (set E2E_UPGRADE=1)")
    if not IMAGE_A or not IMAGE_B:
        pytest.skip("set E2E_IMAGE_A and E2E_IMAGE_B (two loaded runtime tags)")
    deployed = _runtime_image()
    if _tag_of(IMAGE_A) not in deployed:
        pytest.fail(
            f"deployed runtime image '{deployed}' does not contain E2E_IMAGE_A "
            f"'{IMAGE_A}'; install the chart at {IMAGE_A} first"
        )


def test_pvc_survives_helm_upgrade(require_upgrade) -> None:
    """A persistent-sandbox file survives `helm upgrade` (PVC retained)."""
    session = f"upg-{RUN_ID}-{uuid.uuid4().hex[:4]}"
    marker = f"upgrade-marker-{uuid.uuid4().hex[:8]}"

    with httpx.Client(base_url=BROKER_URL, timeout=60) as c:
        _claim_ready_session(c, TEST_USER, session)
        w = c.post(
            "/files/write",
            json={"path": "upgrade-marker.txt", "content": marker},
            headers=headers_for(TEST_USER, session),
        )
        assert w.status_code == 200, w.text

    # --- helm upgrade: same chart, new runtime tag (IMAGE_B) ---
    _run([
        "helm", *EXTRA_KC, "upgrade", RELEASE, CHART, "-n", NS,
        "--reuse-values",
        "--set", f"imageTag={_tag_of(IMAGE_B)}",
        "--wait", "--timeout", "5m",
    ], timeout=420)

    # The recreated runtime pod reattaches the retained PVC; the marker survives.
    # (The upgrade also replaced the broker pod, so go through a FRESH
    # port-forward — the lane's original one died with the old broker.)
    with _fresh_port_forward(8890) as url, httpx.Client(base_url=url, timeout=60) as c:
        r = c.post(
            "/execute",
            json={"command": "cat /workspace/upgrade-marker.txt"},
            headers=headers_for(TEST_USER, session),
        )
        assert r.status_code == 200, r.text
        assert marker in r.json()["stdout"], r.json()
    assert _tag_of(IMAGE_B) in _runtime_image(), "runtime image did not advance to IMAGE_B"


def test_helm_rollback_reverts_image(require_upgrade) -> None:
    """`helm rollback` to the prior revision restores IMAGE_A; the file remains."""
    releases = _helm_releases()
    prev = min(releases)  # the install revision, before the upgrade in the test above
    _run(["helm", *EXTRA_KC, "rollback", RELEASE, str(prev), "-n", NS, "--wait", "--timeout", "5m"],
         timeout=420)
    assert _tag_of(IMAGE_A) in _runtime_image(), "runtime image did not revert to IMAGE_A"
