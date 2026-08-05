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
# Unique suffix per pytest invocation so repeated runs never collide on session keys.
RUN_ID = uuid.uuid4().hex[:8]
# Generous ceiling for the first sandbox to go warm→claimed→Ready under runc.
CLAIM_TIMEOUT = int(os.environ.get("E2E_CLAIM_TIMEOUT", "180"))


def headers(session: str) -> dict[str, str]:
    """Auth headers the broker requires on proxied requests."""
    return {
        "Authorization": f"Bearer {BROKER_SECRET}",
        "X-User-Id": TEST_USER,
        "X-Session-Id": session,
    }


@pytest.fixture(scope="session")
def ready_session() -> str:
    """Claim one sandbox and wait until it answers /execute successfully."""
    session = f"e2e-{RUN_ID}"
    deadline = time.time() + CLAIM_TIMEOUT
    last = "no attempt yet"
    with httpx.Client(base_url=BROKER_URL, timeout=30) as probe:
        while time.time() < deadline:
            try:
                r = probe.post("/execute", json={"command": "echo ready"}, headers=headers(session))
                if r.status_code == 200 and r.json().get("exit_code") == 0:
                    return session
                last = f"HTTP {r.status_code}: {r.text[:200]}"
            except Exception as exc:  # broker/router/proxy not up yet
                last = repr(exc)
            time.sleep(3)
    pytest.fail(f"sandbox never became ready within {CLAIM_TIMEOUT}s (last: {last})")


@pytest.fixture
def broker(ready_session) -> Iterator[httpx.Client]:
    """An httpx client authenticated for the claimed session."""
    with httpx.Client(base_url=BROKER_URL, timeout=60, headers=headers(ready_session)) as c:
        yield c
