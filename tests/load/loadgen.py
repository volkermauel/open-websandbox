# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Async soak/load driver for the open-websandbox broker (issue #127).

Drives the broker-agnostic HTTP/WS surface (same contract as tests/e2e) with N
concurrent virtual users, each owning its own sandbox session:

* ``POST /execute`` — shell round-trip
* ``POST /files/write`` + ``GET /files/read`` — file round-trip
* ``WS /api/terminals/{session}`` — PTY round-trip (auth frame → resize →
  binary stdin → binary stdout marker)

No dependencies beyond the pinned e2e tooling (httpx + websockets); see
``requirements-test.txt``.

Examples
--------
# 30 users, 2 minutes, mixed workload against a port-forwarded broker:
BROKER_URL=http://localhost:8889 BROKER_SECRET=<secret> \
  python3 tests/load/loadgen.py --users 30 --duration 120

# The 10k-WS exercise (issue #126): terminals only, no think time:
  python3 tests/load/loadgen.py --users 10000 --duration 300 --ws-only --think 0

Exit code is non-zero when the error rate exceeds ``--max-error-rate``.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import time
import uuid
from dataclasses import dataclass, field

import httpx
import websockets

BROKER_URL = os.environ.get("BROKER_URL", "http://localhost:8889").rstrip("/")
BROKER_WS = os.environ.get("BROKER_WS") or ("ws" + BROKER_URL[len("http"):])
BROKER_SECRET = os.environ.get("BROKER_SECRET", "dev-shared-secret-change-me")
TEST_USER = os.environ.get("E2E_USER", "load-user")
RUN_ID = uuid.uuid4().hex[:8]
_claim_env = os.environ.get("E2E_CLAIM_TIMEOUT", "180")
try:
    CLAIM_TIMEOUT = int(_claim_env)
except (TypeError, ValueError):
    CLAIM_TIMEOUT = 180


def _headers(session: str) -> dict[str, str]:
    return {
        "Authorization": f"Bearer {BROKER_SECRET}",
        "X-User-Id": TEST_USER,
        "X-Session-Id": session,
    }


@dataclass
class Stats:
    """Per-operation latency (s) and error tallies; mutated by workers."""

    lat: dict[str, list[float]] = field(default_factory=lambda: {"claim": [], "execute": [], "files": [], "ws": []})
    ok: dict[str, int] = field(default_factory=lambda: {"claim": 0, "execute": 0, "files": 0, "ws": 0})
    err: dict[str, int] = field(default_factory=lambda: {"claim": 0, "execute": 0, "files": 0, "ws": 0})

    def record(self, op: str, dt: float, success: bool) -> None:
        if success:
            self.ok[op] += 1
            self.lat[op].append(dt)
        else:
            self.err[op] += 1


def _pct(values: list[float], p: float) -> float:
    if not values:
        return math.nan
    s = sorted(values)
    return s[min(len(s) - 1, round(p / 100 * (len(s) - 1)))]


async def claim(client: httpx.AsyncClient, session: str, stats: Stats) -> None:
    """Poll /execute until the (user, session) sandbox answers."""
    t0 = time.monotonic()
    deadline = t0 + CLAIM_TIMEOUT
    last = "no attempt yet"
    while time.monotonic() < deadline:
        try:
            r = await client.post(
                "/execute", json={"command": "echo ready"}, headers=_headers(session)
            )
            if r.status_code == 200 and r.json().get("exit_code") == 0:
                stats.record("claim", time.monotonic() - t0, True)
                return
            last = f"HTTP {r.status_code}: {r.text[:120]}"
        except Exception as exc:  # noqa: BLE001 — transient broker/proxy unavailability
            last = repr(exc)
        await asyncio.sleep(3)
    stats.record("claim", time.monotonic() - t0, False)
    raise RuntimeError(
        f"session {session} never became ready in {CLAIM_TIMEOUT}s (last: {last})"
    )


async def op_execute(client: httpx.AsyncClient, session: str) -> None:
    marker = f"load-{uuid.uuid4().hex[:8]}"
    r = await client.post(
        "/execute", json={"command": f"echo {marker}"}, headers=_headers(session)
    )
    body = r.json()
    if r.status_code != 200 or marker not in body.get("stdout", ""):
        raise RuntimeError(f"execute mismatch: HTTP {r.status_code}")


async def op_files(client: httpx.AsyncClient, session: str) -> None:
    name = f"load-{uuid.uuid4().hex[:8]}.txt"
    payload = f"payload-{uuid.uuid4().hex[:8]}"
    w = await client.post(
        "/files/write", json={"path": name, "content": payload}, headers=_headers(session)
    )
    if w.status_code != 200:
        raise RuntimeError(f"files/write HTTP {w.status_code}")
    r = await client.get("/files/read", params={"path": name}, headers=_headers(session))
    if r.status_code != 200 or payload not in r.json().get("content", ""):
        raise RuntimeError(f"files/read mismatch: HTTP {r.status_code}")


async def op_ws(session: str) -> None:
    """Full PTY round-trip: auth frame → resize → binary stdin → marker stdout."""
    marker = f"wsm-{uuid.uuid4().hex[:8]}"
    url = f"{BROKER_WS}/api/terminals/{session}"
    async with websockets.connect(
        url,
        additional_headers=_headers(session),
        open_timeout=10,
        close_timeout=5,
    ) as ws:
        await ws.send(json.dumps({"type": "auth", "token": BROKER_SECRET}))
        await ws.send(json.dumps({"type": "resize", "rows": 24, "cols": 80}))
        await ws.send(f"echo {marker}\n".encode())  # PTY stdin rides binary frames
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            frame = await asyncio.wait_for(ws.recv(), timeout=deadline - time.monotonic())
            if isinstance(frame, bytes) and marker.encode() in frame:
                return
        raise RuntimeError("ws marker never echoed")


async def worker(idx: int, stats: Stats, stop: float, args: argparse.Namespace) -> None:
    """One virtual user: own session + client, mixed op loop until `stop`."""
    session = f"load-{RUN_ID}-{idx}"
    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=30) as client:
        try:
            await claim(client, session, stats)
        except RuntimeError:
            return
        while time.monotonic() < stop:
            t0 = time.monotonic()
            try:
                if args.ws_only:
                    await op_ws(session)
                    stats.record("ws", time.monotonic() - t0, True)
                else:
                    roll = uuid.uuid4().int % 6  # execute 3/6, files 2/6, ws 1/6
                    if roll < 3:
                        await op_execute(client, session)
                        stats.record("execute", time.monotonic() - t0, True)
                    elif roll < 5:
                        await op_files(client, session)
                        stats.record("files", time.monotonic() - t0, True)
                    else:
                        await op_ws(session)
                        stats.record("ws", time.monotonic() - t0, True)
            except Exception:  # noqa: BLE001 — a load op failing is data, not a crash
                stats.record("ws" if args.ws_only else "execute", time.monotonic() - t0, False)
            if args.think:
                await asyncio.sleep(args.think)


async def run(args: argparse.Namespace) -> int:
    stop = time.monotonic() + args.duration
    stats = Stats()
    t0 = time.monotonic()
    workers = [asyncio.create_task(worker(i, stats, stop, args)) for i in range(args.users)]
    await asyncio.gather(*workers)
    wall = time.monotonic() - t0

    total_ok = sum(stats.ok.values())
    total_err = sum(stats.err.values())
    total = total_ok + total_err
    print(f"\n=== load summary: {args.users} users, {wall:.0f}s wall ===")
    print(f"{'op':<9}{'ok':>8}{'err':>6}{'p50':>8}{'p95':>8}{'p99':>8}  (s)")
    for op in ("claim", "execute", "files", "ws"):
        lat = stats.lat[op]
        if stats.ok[op] or stats.err[op]:
            print(
                f"{op:<9}{stats.ok[op]:>8}{stats.err[op]:>6}"
                f"{_pct(lat, 50):>8.3f}{_pct(lat, 95):>8.3f}{_pct(lat, 99):>8.3f}"
            )
    err_rate = total_err / total if total else 1.0
    print(f"throughput: {total_ok / wall:.1f} ops/s   error rate: {err_rate:.2%}")
    if args.csv:
        try:
            with open(args.csv, "w", encoding="utf-8") as f:
                f.write("op,ok,err,p50_s,p95_s,p99_s\n")
                for op in ("claim", "execute", "files", "ws"):
                    lat = stats.lat[op]
                    f.write(
                        f"{op},{stats.ok[op]},{stats.err[op]},"
                        f"{_pct(lat, 50):.4f},{_pct(lat, 95):.4f},{_pct(lat, 99):.4f}\n"
                    )
        except OSError as exc:
            raise SystemExit(f"cannot write --csv {args.csv}: {exc}") from exc
        print(f"wrote {args.csv}")
    return 1 if err_rate > args.max_error_rate else 0


def main() -> None:
    p = argparse.ArgumentParser(description="Async soak/load driver for the open-websandbox broker.")
    p.add_argument("--users", type=int, default=20, help="concurrent virtual users (sessions)")
    p.add_argument("--duration", type=int, default=60, help="seconds to run")
    p.add_argument("--think", type=float, default=0.5, help="per-user think time (s) between ops")
    p.add_argument("--ws-only", action="store_true", help="terminals only (10k-WS exercise)")
    p.add_argument("--csv", help="optional path for a per-op CSV summary")
    p.add_argument("--max-error-rate", type=float, default=0.05, help="fail above this error rate")
    args = p.parse_args()
    raise SystemExit(asyncio.run(run(args)))


if __name__ == "__main__":
    main()
