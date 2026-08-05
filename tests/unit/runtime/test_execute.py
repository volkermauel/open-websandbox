"""Tests for ``POST /execute`` — real ``asyncio.create_subprocess_shell``.

Covers: exit codes, stdout/stderr capture and split, the ``_cap`` truncation at
MAX_OUT (monkeypatched small so we don't generate 1 MiB), and the timeout path
(exit_code 124, timed_out=True) including a whole-process-group kill that leaves
no orphaned child behind.

``/bin/sh`` is dash on this host, so every command below is POSIX-portable (no
bash brace expansion, no ``[[ ]]``).
"""

from __future__ import annotations

import asyncio
import subprocess

import server  # type: ignore[import-not-found]  # resolved via conftest sys.path insert


async def test_execute_exit_code_nonzero(workdir, client):
    r = await client.post("/execute", json={"command": "exit 7"})
    assert r.status_code == 200
    body = r.json()
    assert body["exit_code"] == 7
    assert not body["timed_out"]


async def test_execute_exit_code_zero(workdir, client):
    r = await client.post("/execute", json={"command": "true"})
    assert r.json()["exit_code"] == 0


async def test_execute_stdout_capture(workdir, client):
    r = await client.post("/execute", json={"command": "echo hello"})
    body = r.json()
    assert body["stdout"] == "hello\n"
    assert body["stderr"] == ""
    assert body["exit_code"] == 0


async def test_execute_stderr_split(workdir, client):
    # stdout and stderr must be captured into separate fields.
    r = await client.post(
        "/execute", json={"command": "echo out; echo err 1>&2"}
    )
    body = r.json()
    assert "out" in body["stdout"] and "err" not in body["stdout"]
    assert "err" in body["stderr"] and "out" not in body["stderr"]


async def test_execute_cwd_is_workdir(workdir, client):
    # /execute runs with cwd = the request base (the patched WORKDIR).
    r = await client.post("/execute", json={"command": "pwd -P"})
    out = r.json()["stdout"].strip()
    import os

    assert out == os.path.realpath(workdir)


async def test_execute_truncates_above_max_out(workdir, client, monkeypatch):
    # Lower the cap so we don't have to generate 1 MiB. _cap reads the module
    # global MAX_OUT at call time, so monkeypatching takes effect immediately.
    max_out = 32
    monkeypatch.setattr(server, "MAX_OUT", max_out)

    # POSIX-portable: emit exactly 2000 'X' bytes on stdout (no newline).
    r = await client.post(
        "/execute", json={"command": "head -c 2000 /dev/zero | tr '\\0' 'X'"}
    )
    body = r.json()
    assert "...[truncated:" in body["stdout"]
    assert f"{2000 - max_out} more bytes" in body["stdout"]
    # and the returned payload is far smaller than the raw 2000 bytes
    assert len(body["stdout"]) < 2000


async def test_execute_at_max_out_is_not_truncated(workdir, client, monkeypatch):
    # Exactly MAX_OUT bytes must NOT be truncated (boundary: len > MAX_OUT).
    max_out = 32
    monkeypatch.setattr(server, "MAX_OUT", max_out)
    r = await client.post(
        "/execute", json={"command": f"head -c {max_out} /dev/zero | tr '\\0' 'X'"}
    )
    body = r.json()
    assert "truncated" not in body["stdout"]
    assert len(body["stdout"]) == max_out


async def test_execute_timeout_returns_124(workdir, client):
    import time

    start = time.monotonic()
    r = await client.post("/execute", json={"command": "sleep 5", "timeout": 1})
    elapsed = time.monotonic() - start
    body = r.json()
    assert body["exit_code"] == 124
    assert body["timed_out"]
    # honoured the short timeout (with slack for scheduling)
    assert elapsed < 4.0


# --- whole-process-group kill: no orphan survives ---------------------------

_ORPHAN_TOKEN = "2718281828"  # unique sleep duration so pgrep can't match noise


def _count_orphans() -> int:
    """Count live processes whose cmdline contains our orphan token."""
    p = subprocess.run(
        ["pgrep", "-f", f"sleep {_ORPHAN_TOKEN}"], capture_output=True, text=True
    )
    # pgrep exits 1 when nothing matches -> empty stdout
    return len([line for line in p.stdout.splitlines() if line.strip()])


async def test_execute_timeout_kills_whole_process_group(workdir, client):
    # Background two long sleeps, then block the shell with a third. A timeout
    # must SIGKILL the entire process group (start_new_session=True), so the
    # backgrounded sleeps must NOT survive as orphans.
    assert _count_orphans() == 0, "pre-existing orphan before test"

    cmd = (
        f"sleep {_ORPHAN_TOKEN} & "
        f"sleep {_ORPHAN_TOKEN} & "
        f"sleep {_ORPHAN_TOKEN}"
    )
    r = await client.post("/execute", json={"command": cmd, "timeout": 1})
    assert r.json()["exit_code"] == 124

    # Give the SIGKILLs a beat to be reaped, then assert no orphan survived.
    for _ in range(20):
        await asyncio.sleep(0.1)
        if _count_orphans() == 0:
            break
    assert _count_orphans() == 0, "orphaned child survived the group-kill"
