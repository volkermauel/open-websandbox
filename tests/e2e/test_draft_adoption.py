"""Draft-adoption e2e (issue #157) — uploads made in an id-less new chat follow the chat.

OWUI v0.11 sends no ``X-Session-Id`` until a new chat is persisted (first
message), so pre-message uploads land in the user-keyed **draft** sandbox
(``owui-c-<sha256(user/user)>`` — the broker defaults the session to the user
id). When the chat id materializes, a fresh chat sandbox would normally show an
empty workspace. Draft adoption moves the draft workspace into the chat's
subPath before readiness returns.

What is proven here (per-user-pvc mode, same gating as test_pvc_persistence):

1. **Upload follows the chat** — a marker written via a session-LESS request
   (draft sandbox) is visible in the chat sandbox after the chat's first
   resolve with a real session id.
2. **Adoption is create-time only** — a second chat of the same user starts
   empty (the draft was consumed), and re-resolving the first chat keeps its
   files.
"""

from __future__ import annotations

import os
import uuid

import httpx
import pytest
from conftest import (
    BROKER_SECRET,
    BROKER_URL,
    CLAIM_TIMEOUT,
    _claim_ready_session,
    headers_for,
)

pytestmark = [
    pytest.mark.usefixtures("require_pvc"),
    pytest.mark.skipif(os.getenv("E2E_PVC") != "1", reason="opt-in: set E2E_PVC=1"),
]


def _draft_headers(user: str) -> dict[str, str]:
    """OWUI's new-chat traffic: auth + user, NO X-Session-Id (no chat id yet)."""
    return {"Authorization": f"Bearer {BROKER_SECRET}", "X-User-Id": user}


def _exec(c: httpx.Client, headers: dict[str, str], command: str) -> tuple[int, str]:
    r = c.post(
        "/execute",
        json={"command": command},
        headers={**headers, "X-Persistence": "persistent"},
    )
    assert r.status_code == 200, f"{r.status_code}: {r.text[:300]}"
    body = r.json()
    return body.get("exit_code", -1), (body.get("stdout") or "")


def test_draft_upload_follows_chat_once_it_gets_an_id():
    user = f"u-draft-{uuid.uuid4().hex[:6]}"
    chat = f"chat-{uuid.uuid4().hex[:8]}"
    marker = f"DRAFT-ADOPT-{uuid.uuid4().hex[:8]}"

    with httpx.Client(base_url=BROKER_URL, timeout=CLAIM_TIMEOUT) as c:
        # 1. New-chat phase: no X-Session-Id → broker keys the sandbox by user
        #    alone (the draft sandbox). "Upload" a file there.
        code, out = _exec(c, _draft_headers(user), f"echo {marker} > /workspace/upload.txt")
        assert code == 0, out

        # 2. The chat is persisted (first message) and gets its id. The first
        #    resolve with a real session id must ADOPT the draft workspace.
        _claim_ready_session(c, user, chat)
        code, out = _exec(c, headers_for(user, chat), "cat /workspace/upload.txt")
        assert code == 0, out
        assert marker in out, f"draft upload did not follow the chat: {out!r}"

        # 3. Adoption is create-time only: a second chat starts empty (the
        #    draft was consumed by the first).
        chat2 = f"chat-{uuid.uuid4().hex[:8]}"
        _claim_ready_session(c, user, chat2)
        code, out = _exec(c, headers_for(user, chat2), "ls /workspace")
        assert code == 0, out
        assert "upload.txt" not in out, f"draft leaked into a second chat: {out!r}"

        # 4. And the first chat keeps its adopted file on re-resolve.
        code, out = _exec(c, headers_for(user, chat), "cat /workspace/upload.txt")
        assert code == 0 and marker in out, f"adopted file lost on re-resolve: {out!r}"
