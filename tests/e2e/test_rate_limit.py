"""#161: rate limits are scoped per chat, with a bounded per-user aggregate.

Two stacked token buckets on the broker's gated surface: per **chat**
(``X-User-Id`` + ``X-Session-Id``) and per **user** (``X-User-Id`` alone,
per-chat budget x multiplier). The test drives a broker deployed with
deliberately tiny buckets (``values-kind-rl.yaml``: 2 rps / burst 5 / x3)
and asserts — value-agnostically, against the ``x-ratelimit-limit`` headers:

  1. hammering ONE chat trips a 429 carrying the per-chat headers,
  2. a sibling chat of the SAME user still passes (independent buckets),
  3. a different user still passes (no cross-user interference),
  4. sustained load across several chats trips the per-USER aggregate: a
     FRESH session (full chat bucket) still 429s — only the user layer can
     reject it — and that 429 carries no x-ratelimit headers (the user layer
     is headerless by design: headers always describe the chat bucket).

Needs a live broker (CI lanes export BROKER_URL/SECRET; every lane exports a
KUBECONFIG — R1 keeps it explicit), so it skips without one.
"""

import concurrent.futures
import os
import threading
import time

import httpx
import pytest
from conftest import (  # noqa: E402
    BROKER_URL,
    TEST_USER,
    headers_for,
)

pytestmark = pytest.mark.skipif(
    not os.getenv("KUBECONFIG") or not BROKER_URL,
    reason="needs a live broker (KIND lane exports KUBECONFIG + BROKER_URL)",
)


def _get(client: httpx.Client, user: str, session: str) -> httpx.Response:
    # GET /api/status is on the gated router and answers from memory (no kube
    # round-trip, no sandbox churn) — the fastest fully rate-limited hammer
    # target, so threads can actually outpace the tiny refill rates.
    return client.get(f"{BROKER_URL}/api/status", headers=headers_for(user, session))


def _hammer_until(user: str, session: str, deadline: float, stop: threading.Event) -> None:
    """Keep one chat bucket saturated until `deadline` or `stop` is set."""
    with httpx.Client(timeout=15) as own:
        while time.monotonic() < deadline and not stop.is_set():
            _get(own, user, session)


def test_rate_limits_are_per_chat_with_user_aggregate():
    user = TEST_USER
    other = f"{TEST_USER}-other"

    with httpx.Client(timeout=15) as client:
        # Baseline: everyone passes.
        for session in ("s1", "s2"):
            assert _get(client, user, session).status_code == 200
        assert _get(client, other, "s1").status_code == 200

        # 1) Hammer chat s1 alone -> its bucket empties; capture the per-chat
        #    limit from the 429 (values-kind-rl.yaml: burst 10, so this trips
        #    within a couple dozen rapid requests).
        chat_limit = None
        for _ in range(60):
            response = _get(client, user, "s1")
            if response.status_code == 429:
                chat_limit = response.headers.get("x-ratelimit-limit")
                assert response.headers.get("retry-after"), "429 must carry Retry-After"
                break
        assert chat_limit, "per-chat bucket must trip when one chat is hammered"

        # 2) Sibling chat of the SAME user still passes.
        sibling = _get(client, user, "s2")
        assert sibling.status_code == 200, "one busy chat starved a sibling chat"

        # 3) A different user is unaffected.
        assert _get(client, other, "s1").status_code == 200

        # 4) Sustained load across several chats exceeds the per-user aggregate
        #    (admitted 4 x refill > aggregate refill): once the user bucket is
        #    drained, even a FRESH session — whose chat bucket is untouched and
        #    full — gets 429'd. Only the user layer can reject a fresh session,
        #    and its 429 carries no x-ratelimit headers (headerless by design,
        #    see rate_limit.rs: headers always describe the chat bucket).
        drained = False
        stop = threading.Event()
        deadline = time.monotonic() + 60
        probe = 0
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            hammers = [
                pool.submit(_hammer_until, user, f"agg-{i}", deadline, stop)
                for i in range(4)
            ]
            with httpx.Client(timeout=15) as own:
                while time.monotonic() < deadline:
                    probe += 1
                    response = _get(own, user, f"probe-{probe}")
                    if response.status_code == 429:
                        assert not response.headers.get("x-ratelimit-limit"), (
                            "fresh-session 429 must be the headerless user layer"
                        )
                        drained = True
                        break
                    time.sleep(0.25)
            stop.set()
            for hammer in hammers:
                hammer.result()
        assert drained, "per-user aggregate never tripped under sustained multi-chat load"
