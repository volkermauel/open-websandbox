"""#164 stage 1: the open-terminal v0.12.3 compatibility surface, driven
end-to-end through the broker relay (the exact path OWUI takes).

Covers: ``GET /files/serve/{path}`` (inline bytes), ``GET /api/config``
(feature discovery), ``/files/list`` writability flags, ``/files/read``
line ranges + 415 binaries, ``GET /files/search`` and ``GET /files/matches``.
Contract-level detail (ranking, tie-breaks, UTF-16 columns) lives in the Rust
contract tests; this file proves the broker forwards the new routes with
auth intact and the round-trips are byte-exact.
"""

import hashlib
import os

import httpx
import pytest
from conftest import (  # noqa: E402
    BROKER_URL,
    TEST_USER,
    _claim_ready_session,
    headers_for,
)

pytestmark = pytest.mark.skipif(
    not os.getenv("BROKER_URL") or not os.getenv("BROKER_SECRET"),
    reason="needs a live broker (KIND lane exports BROKER_URL + BROKER_SECRET)",
)

CLAIM_TIMEOUT = 300.0


def test_serve_returns_inline_bytes_through_the_relay():
    session = f"ott-serve-{hashlib.sha256(b'serve').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        up = client.post(
            "/files/write",
            json={"path": "site/index.html", "content": "<h1>hello serve</h1>"},
            headers=headers_for(TEST_USER, session),
        )
        assert up.status_code == 200, up.text[:200]

        resp = client.get(
            "/files/serve/site/index.html", headers=headers_for(TEST_USER, session)
        )
        assert resp.status_code == 200, resp.text[:200]
        assert resp.headers["content-type"].startswith("text/html")
        # Inline serving (iframe-ready): no attachment disposition.
        assert "content-disposition" not in resp.headers
        assert resp.content == b"<h1>hello serve</h1>"

        # Unauthenticated through the relay fails closed.
        unauth = client.get("/files/serve/site/index.html")
        assert unauth.status_code == 401


def test_api_config_reports_v0_12_3_features():
    session = f"ott-cfg-{hashlib.sha256(b'cfg').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        resp = client.get("/api/config", headers=headers_for(TEST_USER, session))
        assert resp.status_code == 200, resp.text[:200]
        features = resp.json()["features"]
        assert features["terminal"] is True
        assert features["notebooks"] is False
        assert features["system"] is False


def test_list_carries_writable_flags_and_read_slices_lines():
    session = f"ott-rw-{hashlib.sha256(b'rw').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        up = client.post(
            "/files/write",
            json={"path": "lines.txt", "content": "one\ntwo\nthree\nfour\n"},
            headers=headers_for(TEST_USER, session),
        )
        assert up.status_code == 200, up.text[:200]

        listing = client.get(
            "/files/list",
            params={"directory": "/workspace"},
            headers=headers_for(TEST_USER, session),
        )
        assert listing.status_code == 200, listing.text[:200]
        doc = listing.json()
        assert doc["writable"] is True
        mine = [e for e in doc["entries"] if e["name"] == "lines.txt"]
        assert mine and mine[0]["writable"] is True

        sliced = client.get(
            "/files/read",
            params={"path": "lines.txt", "start_line": 2, "end_line": 3},
            headers=headers_for(TEST_USER, session),
        )
        assert sliced.status_code == 200, sliced.text[:200]
        body = sliced.json()
        assert body["total_lines"] == 4
        assert body["content"] == "two\nthree\n"


def test_read_rejects_binary_with_415_instead_of_500():
    session = f"ott-415-{hashlib.sha256(b'415').hexdigest()[:8]}"
    payload = bytes([0x1F, 0x8B, 0x08, 0x00]) + bytes(64)
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        up = client.post(
            "/files/upload",
            params={"directory": "/workspace"},
            headers=headers_for(TEST_USER, session),
            files={"file": ("blob.bin", payload, "application/octet-stream")},
        )
        assert up.status_code == 200, up.text[:200]

        resp = client.get(
            "/files/read", params={"path": "blob.bin"}, headers=headers_for(TEST_USER, session)
        )
        assert resp.status_code == 415, resp.text[:200]
        assert "Unsupported binary file type" in resp.text


def test_search_and_matches_find_uploaded_files():
    session = f"ott-srch-{hashlib.sha256(b'srch').hexdigest()[:8]}"
    marker = "quokka-settler-42"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        for name in ("needle_alpha.txt", "docs/needle_beta.md"):
            up = client.post(
                "/files/write",
                json={"path": name, "content": f"plain text {marker} line\n"},
                headers=headers_for(TEST_USER, session),
            )
            assert up.status_code == 200, up.text[:200]

        found = client.get(
            "/files/search",
            params={"query": "needle"},
            headers=headers_for(TEST_USER, session),
        )
        assert found.status_code == 200, found.text[:200]
        names = [r["name"] for r in found.json()["results"]]
        assert "needle_alpha.txt" in names and "needle_beta.md" in names

        hits = client.get(
            "/files/matches",
            params={"query": marker},
            headers=headers_for(TEST_USER, session),
        )
        assert hits.status_code == 200, hits.text[:200]
        results = hits.json()["results"]
        assert len(results) == 2, results
        for row in results:
            # Content-only match: the file names do not contain the marker.
            assert row["name_match"] is False
            assert row["content_matches"][0]["text"].endswith(f"{marker} line")

        blank = client.get(
            "/files/matches", params={"query": "  "}, headers=headers_for(TEST_USER, session)
        )
        assert blank.status_code == 400
