# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Shared fixtures for the KIND/runc end-to-end suite.

These tests run against a *live* broker (installed via the Helm chart into a KIND
cluster, or any reachable deployment). Configure with env vars:

    BROKER_URL      http://localhost:8889   (after `kubectl port-forward`)
    BROKER_SECRET   <broker.sharedSecret>   (default: the chart's dev secret)
    E2E_USER        e2e-user

A single sandbox (one session) is claimed once for the whole run (session-scoped) so
the tests are fast and can assert cross-request persistence. The claim polls because
the warm pool + first Sandbox claim take a few seconds to become Ready.
"""
import os
import time
import uuid
from collections.abc import Iterator

import httpx
import pytest

BROKER_URL = os.environ.get("BROKER_URL", "http://localhost:8889").rstrip("/")
BROKER_SECRET = os.environ.get("BROKER_SECRET", "dev-shared-secret-change-me")
TEST_USER = os.environ.get("E2E_USER", "e2e-user")
SECOND_USER = os.environ.get("E2E_USER_2", "e2e-user-2")
# Unique suffix per pytest invocation so repeated runs never collide on session keys.
RUN_ID = uuid.uuid4().hex[:8]
# Generous ceiling for the first sandbox to go warm→claimed→Ready under runc.
_claim_env = os.environ.get("E2E_CLAIM_TIMEOUT", "180")
try:
    CLAIM_TIMEOUT = int(_claim_env)
except (TypeError, ValueError):
    CLAIM_TIMEOUT = 180


def headers_for(user_id: str, session: str) -> dict[str, str]:
    """Auth headers for an arbitrary (user, session) pair — multi-tenant tests."""
    return {
        "Authorization": f"Bearer {BROKER_SECRET}",
        "X-User-Id": user_id,
        "X-Session-Id": session,
    }


def headers(session: str) -> dict[str, str]:
    """Auth headers the broker requires on proxied requests (default tenant)."""
    return headers_for(TEST_USER, session)


def _claim_ready_session(probe: httpx.Client, user_id: str, session: str) -> str:
    """Poll /execute until the sandbox for (user, session) answers successfully."""
    deadline = time.time() + CLAIM_TIMEOUT
    last = "no attempt yet"
    while time.time() < deadline:
        try:
            r = probe.post(
                "/execute",
                json={"command": "echo ready"},
                headers=headers_for(user_id, session),
            )
            if r.status_code == 200 and r.json().get("exit_code") == 0:
                return session
            last = f"HTTP {r.status_code}: {r.text[:200]}"
        except Exception as exc:  # broker/router/proxy not up yet
            last = repr(exc)
        time.sleep(3)
    pytest.fail(f"sandbox never became ready within {CLAIM_TIMEOUT}s (last: {last})")


@pytest.fixture(scope="session")
def require_broker() -> None:
    """Skip the suite early when the broker isn't reachable (no live cluster).

    Negative/isolation tests request this fixture FIRST so they skip fast instead
    of burning CLAIM_TIMEOUT on a sandbox that can never be claimed.
    """
    try:
        with httpx.Client(base_url=BROKER_URL, timeout=5) as probe:
            r = probe.get("/healthz")
        if r.status_code != 200:
            pytest.skip(f"broker unhealthy at {BROKER_URL} (HTTP {r.status_code})")
    except Exception as exc:
        pytest.skip(f"broker not reachable at {BROKER_URL}: {exc!r}")


@pytest.fixture(scope="session")
def ready_session() -> str:
    """Claim one sandbox (default tenant) and wait until it answers /execute."""
    session = f"e2e-{RUN_ID}"
    with httpx.Client(base_url=BROKER_URL, timeout=30) as probe:
        return _claim_ready_session(probe, TEST_USER, session)


@pytest.fixture(scope="session")
def second_session() -> str:
    """Claim a sandbox for a DIFFERENT tenant (cross-tenant isolation tests)."""
    session = f"e2e2-{RUN_ID}"
    with httpx.Client(base_url=BROKER_URL, timeout=30) as probe:
        return _claim_ready_session(probe, SECOND_USER, session)


@pytest.fixture
def broker(ready_session) -> Iterator[httpx.Client]:
    """An httpx client authenticated for the claimed session."""
    with httpx.Client(base_url=BROKER_URL, timeout=60, headers=headers(ready_session)) as c:
        yield c


@pytest.fixture
def second_broker(second_session) -> Iterator[httpx.Client]:
    """An httpx client authenticated as a SECOND tenant (different user + session)."""
    with httpx.Client(
        base_url=BROKER_URL, timeout=60, headers=headers_for(SECOND_USER, second_session)
    ) as c:
        yield c


# --- S3-tiered e2e (issue #52) ------------------------------------------------
# The s3-tiered e2e is opt-in (E2E_S3=1; the default runc/gvisor matrix is
# unaffected). It stands up an in-cluster MinIO (tests/e2e/fixtures/minio.yaml),
# points the broker at it, and inspects objects with boto3 run LOCALLY against a
# port-forwarded MinIO. The Rust broker image is distroless (no Python/shell), so
# the original "exec boto3 inside the broker pod" trick no longer applies;
# instead the test host runs boto3 + reads the same creds the broker uses.
import base64  # noqa: E402
import subprocess  # noqa: E402
import urllib.request  # noqa: E402

S3_SYS_NS = os.environ.get("E2E_SYS_NS", "agent-sandbox-system")
S3_BUCKET = os.environ.get("E2E_S3_BUCKET", "owsb-e2e")
# MinIO is reached from the test host via port-forward (in-cluster DNS does not
# resolve on the host). 9000 matches the s3 port in fixtures/minio.yaml.
S3_PF_PORT = int(os.environ.get("E2E_S3_PF_PORT", "9000"))


def _s3_creds() -> tuple[str, str]:
    """Read the MinIO root creds from the owui-s3-creds Secret."""
    def _val(key: str) -> str:
        r = subprocess.run(
            ["kubectl", "-n", S3_SYS_NS, "get", "secret", "owui-s3-creds",
             "-o", f"jsonpath={{.data.{key}}}"],
            capture_output=True, text=True, timeout=30,
        )
        if r.returncode != 0 or not r.stdout:
            raise RuntimeError(f"owui-s3-creds missing key {key}: {r.stderr[-200:]}")
        return base64.b64decode(r.stdout).decode().strip()
    return _val("access-key-id"), _val("secret-access-key")


def _s3_client():
    """A boto3 S3 client pointed at the port-forwarded MinIO (path-style)."""
    import boto3
    from botocore.config import Config
    ak, sk = _s3_creds()
    return boto3.client(
        "s3",
        endpoint_url=f"http://localhost:{S3_PF_PORT}",
        aws_access_key_id=ak,
        aws_secret_access_key=sk,
        region_name="us-east-1",
        config=Config(s3={"addressing_style": "path"}, signature_version="s3v4"),
    )


@pytest.fixture(scope="session")
def minio_port_forward():
    """Port-forward svc/minio to localhost for the s3-tiered session."""
    pf = subprocess.Popen(
        ["kubectl", "-n", S3_SYS_NS, "port-forward", "svc/minio",
         f"{S3_PF_PORT}:9000"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(30):
            try:
                with urllib.request.urlopen(
                    f"http://localhost:{S3_PF_PORT}/minio/health/live", timeout=2,
                ):
                    break
            except Exception:
                time.sleep(1)
        else:
            raise RuntimeError(f"MinIO port-forward :{S3_PF_PORT} never became healthy")
        yield
    finally:
        pf.terminate()
        try:
            pf.wait(timeout=10)
        except subprocess.TimeoutExpired:
            pf.kill()


@pytest.fixture(scope="session")
def require_s3(request) -> None:
    """Gate the s3-tiered e2e: skip unless E2E_S3=1. Ensures the bucket exists.

    The gate MUST run before the MinIO port-forward is requested: taking
    `minio_port_forward` as a parameter would instantiate it first (pytest
    resolves dependencies before the fixture body), so lanes without in-cluster
    MinIO (the runc/gvisor matrix) would spend 30s on a dead port-forward and
    ERROR instead of skipping. Resolve it lazily, only after the gate passes.
    """
    if not os.environ.get("E2E_S3"):
        pytest.skip("S3-tiered e2e is opt-in (set E2E_S3=1)")
    request.getfixturevalue("minio_port_forward")
    from botocore.exceptions import ClientError
    c = _s3_client()
    try:
        c.create_bucket(Bucket=S3_BUCKET)
    except ClientError as e:
        code = e.response.get("Error", {}).get("Code", "")
        if code not in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
            raise


def minio_list_objects(prefix: str = "users/") -> list[str]:
    """List object keys under `prefix` in the e2e MinIO bucket (local boto3)."""
    r = _s3_client().list_objects_v2(Bucket=S3_BUCKET, Prefix=prefix)
    return [o["Key"] for o in r.get("Contents", [])]

