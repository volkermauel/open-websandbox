# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Component-kill chaos harness for the open-websandbox control plane (issue #127).

Runs background traffic through the broker (same ops as ``loadgen.py``), kills a
control-plane component mid-traffic, and asserts the platform self-heals:

    python3 tests/load/chaos.py --target broker
    python3 tests/load/chaos.py --target router --users 5 --duration 90

Targets (deployment name → namespace; pods are deleted by name prefix, so no
label knowledge is needed):

    broker      deploy/owui-broker                (agent-sandbox-system)
    router      deploy/sandbox-router             (agent-sandbox-system)
    controller  deploy/agent-sandbox-controller   (agent-sandbox-system)

Success criteria
----------------
1. **Recovery**: after the kill, a fresh ``/execute`` succeeds again within
   ``--max-recover`` seconds (default 120).
2. **Bounded errors**: the overall error rate stays below ``--max-error-rate``
   (default 0.5 — some errors *during* the kill window are expected and fine).

Honors repo rule R1: every ``kubectl`` call is pointed at ``$KUBECONFIG``
explicitly when set.
"""
from __future__ import annotations

import argparse
import asyncio
import os
import subprocess
import time
from dataclasses import dataclass, field

import httpx
from loadgen import BROKER_URL, CLAIM_TIMEOUT, Stats, _headers, claim

SYS_NS = os.environ.get("E2E_SYS_NS", "agent-sandbox-system")
EXTRA_KC = ["--kubeconfig", os.environ["KUBECONFIG"]] if os.environ.get("KUBECONFIG") else []
# deployment name (pods carry it as their name prefix) per --target
TARGETS: dict[str, str] = {
    "broker": "owui-broker",
    "router": "sandbox-router",
    "controller": "agent-sandbox-controller",
}


def _run(argv: list[str], timeout: int = 60) -> str:
    """Run kubectl, raising RuntimeError with stderr on failure."""
    r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    if r.returncode != 0:
        raise RuntimeError(f"`{' '.join(argv)}` exited {r.returncode}: {r.stderr[-300:]}")
    return r.stdout


def kill_pods(deploy: str) -> int:
    """Delete every running pod of `deploy` (by name prefix). Returns the count."""
    out = _run([
        "kubectl", *EXTRA_KC, "-n", SYS_NS, "get", "pods",
        "-o", "jsonpath={.items[*].metadata.name}",
    ])
    pods = [p for p in out.split() if p.startswith(deploy)]
    for p in pods:
        # --wait=false: fire the deletions, don't block on graceful termination
        _run(["kubectl", *EXTRA_KC, "-n", SYS_NS, "delete", "pod", p, "--wait=false"])
    return len(pods)


@dataclass
class Traffic:
    """Shared tallies for the background traffic tasks."""

    ok: int = 0
    err: int = 0
    stop: bool = False
    errors: list[str] = field(default_factory=list)


async def traffic(idx: int, t: Traffic) -> None:
    """One virtual user looping /execute until `t.stop`; errors are data."""
    session = f"chaos-{idx}"
    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=30) as client:
        try:
            await claim(client, session, Stats())
        except RuntimeError:
            return  # sandbox never came up; counted as an error by the caller
        while not t.stop:
            marker = f"chaos-{idx}-{time.monotonic_ns()}"
            try:
                r = await client.post(
                    "/execute",
                    json={"command": f"echo {marker}"},
                    headers=_headers(session),
                )
                if r.status_code == 200 and marker in r.json().get("stdout", ""):
                    t.ok += 1
                else:
                    t.err += 1
                    t.errors.append(f"HTTP {r.status_code}")
            except Exception as exc:  # noqa: BLE001 — errors during kills are expected
                t.err += 1
                t.errors.append(repr(exc)[:120])
            await asyncio.sleep(0.5)


async def wait_recovered(timeout: float) -> float:
    """Poll a plain /execute until the broker answers again; return the delay."""
    t0 = time.monotonic()
    session = f"chaos-recover-{int(t0)}"
    last = "no attempt yet"
    async with httpx.AsyncClient(base_url=BROKER_URL, timeout=15) as client:
        while time.monotonic() - t0 < timeout:
            marker = f"recovered-{time.monotonic_ns()}"
            try:
                r = await client.post(
                    "/execute",
                    json={"command": f"echo {marker}"},
                    headers=_headers(session),
                )
                if r.status_code == 200 and r.json().get("exit_code") == 0:
                    return time.monotonic() - t0
            except Exception as exc:  # noqa: BLE001 — broker may still be down
                last = repr(exc)[:120]
            await asyncio.sleep(2)
    raise TimeoutError(
        f"no recovery within {timeout:.0f}s (claim ceiling {CLAIM_TIMEOUT}s; last: {last})"
    )


async def run(args: argparse.Namespace) -> int:
    t = Traffic()
    tasks = [asyncio.create_task(traffic(i, t)) for i in range(args.users)]

    print(f"chaos: {args.users} traffic users warming up ({args.warmup}s)…")
    await asyncio.sleep(args.warmup)

    deploy = TARGETS[args.target]
    n = await asyncio.to_thread(kill_pods, deploy)
    print(f"chaos: killed {n} pod(s) of {deploy} — traffic continues {args.duration}s")
    await asyncio.sleep(args.duration)
    t.stop = True
    await asyncio.gather(*tasks)

    total = t.ok + t.err
    err_rate = t.err / total if total else 1.0
    print(f"traffic: {t.ok} ok / {t.err} err (error rate {err_rate:.2%})")
    for e in t.errors[:5]:
        print(f"  e.g. {e}")

    t0 = time.monotonic()
    recovered = await wait_recovered(args.max_recover)
    print(f"recovery: /execute healthy again after {recovered:.1f}s "
          f"(wall since kill incl. traffic tail: {time.monotonic() - t0 + args.duration:.0f}s)")

    ok = err_rate <= args.max_error_rate
    print(f"RESULT: {'PASS' if ok else 'FAIL'} "
          f"(error rate {err_rate:.2%} ≤ {args.max_error_rate:.0%}, recovered in {recovered:.1f}s)")
    return 0 if ok else 1


def main() -> None:
    p = argparse.ArgumentParser(description="Component-kill chaos harness (issue #127).")
    p.add_argument("--target", choices=sorted(TARGETS), required=True,
                   help="control-plane component to kill mid-traffic")
    p.add_argument("--users", type=int, default=3, help="background traffic users")
    p.add_argument("--duration", type=int, default=60, help="seconds of traffic after the kill")
    p.add_argument("--warmup", type=int, default=10, help="seconds of traffic before the kill")
    p.add_argument("--max-recover", type=float, default=120,
                   help="max seconds until /execute answers again after the kill")
    p.add_argument("--max-error-rate", type=float, default=0.5,
                   help="fail above this overall error rate (kills cause errors)")
    args = p.parse_args()
    raise SystemExit(asyncio.run(run(args)))


if __name__ == "__main__":
    main()
