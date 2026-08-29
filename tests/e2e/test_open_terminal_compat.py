"""#164 stage 1 + #169 stage 2: the open-terminal v0.12.3 compatibility
surface, driven end-to-end through the broker relay (the exact path OWUI
takes).

Stage 1: ``GET /files/serve/{path}`` (inline bytes), ``GET /api/config``
(feature discovery), ``/files/list`` writability flags, ``/files/read``
line ranges + 415 binaries, ``GET /files/search`` and ``GET /files/matches``.

Stage 2: ``GET /system`` (upstream-verbatim LLM prompt), ``GET /info``
(404 while unset, like upstream's conditional registration),
``GET /files/display`` (show-file signaling), real ``GET /ports`` and the
``/proxy/{port}`` 0.12.2 ownership lockdown (the runtime's own :8888 must
NOT be proxyable — it is not a descendant of itself).

Contract-level detail (prompt byte-exactness, happy-path proxying, UTF-16
columns) lives in the Rust contract tests; this file proves the broker
forwards the new routes with auth intact.
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
        # Flipped in stage 2 (#169): GET /system is now served.
        assert features["system"] is True


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


# --- #169 stage 2 -------------------------------------------------------------


def test_system_returns_upstream_prompt_through_the_relay():
    session = f"ott-sys-{hashlib.sha256(b'sys').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        resp = client.get("/system", headers=headers_for(TEST_USER, session))
        assert resp.status_code == 200, resp.text[:200]
        prompt = resp.json()["prompt"]
        # Upstream-verbatim opening + closing (values grounded in the pod).
        assert prompt.startswith("You have access to a computer running Linux ")
        upstream_tail = (
            "If a command produces no output, that typically means it succeeded."
        )
        assert upstream_tail in prompt
        # Workbench knob (SANDBOX_TOOLS_MANIFEST, default-on) appends AFTER the
        # upstream prompt — the upstream text stays verbatim and comes first.
        assert prompt.index(upstream_tail) < prompt.index("## Available toolchain")

        unauth = client.get("/system")
        assert unauth.status_code == 401


def test_info_404s_like_upstreams_unregistered_route():
    # The chart sets no OPEN_TERMINAL_INFO, so the route 404s exactly like
    # upstream's `if OPEN_TERMINAL_INFO:` conditional registration.
    session = f"ott-info-{hashlib.sha256(b'info').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        resp = client.get("/info", headers=headers_for(TEST_USER, session))
        assert resp.status_code == 404, resp.text[:200]
        assert resp.json() == {"detail": "Not Found"}


def test_display_signals_file_existence():
    session = f"ott-disp-{hashlib.sha256(b'disp').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)
        up = client.post(
            "/files/write",
            json={"path": "preview.md", "content": "# shown"},
            headers=headers_for(TEST_USER, session),
        )
        assert up.status_code == 200, up.text[:200]

        shown = client.get(
            "/files/display",
            params={"path": "preview.md"},
            headers=headers_for(TEST_USER, session),
        )
        assert shown.status_code == 200, shown.text[:200]
        body = shown.json()
        assert body["exists"] is True
        assert body["path"].endswith("preview.md")

        missing = client.get(
            "/files/display",
            params={"path": "nope.md"},
            headers=headers_for(TEST_USER, session),
        )
        assert missing.status_code == 200
        assert missing.json()["exists"] is False


def test_ports_shape_and_proxy_lockdown():
    session = f"ott-proxy-{hashlib.sha256(b'proxy').hexdigest()[:8]}"
    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as client:
        _claim_ready_session(client, TEST_USER, session)

        ports = client.get("/ports", headers=headers_for(TEST_USER, session))
        assert ports.status_code == 200, ports.text[:200]
        rows = ports.json()["ports"]
        assert isinstance(rows, list)
        for row in rows:
            assert isinstance(row["port"], int)
            assert "uid" not in row, "uid stripped like upstream"

        # The runtime's own :8888 listener is owned by the runtime process
        # itself — NOT a descendant — so the 0.12.2 lockdown must 404 it.
        own = client.get("/proxy/8888/", headers=headers_for(TEST_USER, session))
        assert own.status_code == 404, own.text[:200]
        assert own.json() == {"detail": "Port not found"}

        # Unlistened port: same upstream 404; port 0: upstream range message
        # (upstream 422 vs our documented 400 divergence).
        unlistened = client.get(
            "/proxy/47000/", headers=headers_for(TEST_USER, session)
        )
        assert unlistened.status_code == 404, unlistened.text[:200]
        assert unlistened.json() == {"detail": "Port not found"}

        zero = client.get("/proxy/0/x", headers=headers_for(TEST_USER, session))
        assert zero.status_code == 400, zero.text[:200]
        assert zero.json() == {"detail": "Port must be between 1 and 65535"}

        unauth = client.get("/proxy/8888/")
        assert unauth.status_code == 401
