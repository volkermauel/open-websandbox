"""#162: uploads above 2 MiB must survive both hops.

Neither the broker proxy nor the runtime raised axum's ``DefaultBodyLimit``,
so its built-in 2 MiB cap rejected multipart upload bodies — files "broke"
above 2 MiB regardless of the 2 GiB workspace quota. The runtime now allows
``MAX_UPLOAD_BYTES`` (default 1 GiB) and the broker's gated surface allows up
to its 256 MiB forward cap.

Drives exactly Open Web UI's path: claim a session, POST a 3 MiB multipart
upload through the BROKER (proxy → runtime), then read it back through the
broker and byte-compare. Runs in the default e2e matrix lanes (``pytest
tests/e2e``).
"""

import hashlib
import os

import httpx
import pytest
from conftest import (  # noqa: E402
    BROKER_URL,
    CLAIM_TIMEOUT,
    TEST_USER,
    _claim_ready_session,
    headers_for,
)

pytestmark = pytest.mark.skipif(
    not os.getenv("KUBECONFIG") or not BROKER_URL,
    reason="needs a live broker (KIND lane exports KUBECONFIG + BROKER_URL)",
)

SIZE = 3 * 1024 * 1024  # comfortably above axum's old 2 MiB default


def test_upload_over_two_mebibytes_round_trips():
    payload = bytes((i * 7 + 3) % 256 for i in range(SIZE))
    digest = hashlib.sha256(payload).hexdigest()
    session = f"upload-{digest[:8]}"

    # base_url is required: conftest's _claim_ready_session posts the
    # relative "/execute" against it (mirrors the broker fixture's client).
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)

        # 1) Upload through the broker (proxy → runtime), like OWUI does.
        response = client.post(
            f"{BROKER_URL}/files/upload",
            params={"directory": "/workspace"},
            headers=headers_for(TEST_USER, session),
            files={"file": ("big.bin", payload, "application/octet-stream")},
        )
        assert response.status_code == 200, response.text[:200]

        # 2) Verify byte-exact content via sha256sum (our /files/read is
        # UTF-8-only — binary-aware read is tracked as an upstream gap).
        back = client.post(
            "/execute",
            json={"command": "sha256sum /workspace/big.bin"},
            headers=headers_for(TEST_USER, session),
        )
        assert back.status_code == 200, back.text[:200]
        assert back.json()["exit_code"] == 0, back.text[:200]
        assert digest in back.json()["stdout"], back.text[:200]
