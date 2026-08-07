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
