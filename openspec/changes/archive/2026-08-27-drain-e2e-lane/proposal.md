# Proposal: drain e2e matrix arm

## Why

`tests/e2e/test_node_drain.py` (issue #129) is env-gated behind `E2E_DRAIN=1`
and never executed in CI. The #144 coverage analysis deferred wiring it up
until the KIND topology was known; the probe verdict: every CI cluster is
single-node, but the test doesn't drain a node — it deletes the sandbox pod
(same blast radius for the pod), and single-node RWO local-path rebinds fine
on the same node. So the lane can run today on the per-user PVC profile.

## What Changes

- `.github/workflows/e2e.yml`: new `drain` arm in the `e2e-pvc` matrix —
  installs `values-kind-pvc.yaml` (default of the helm-install step) and runs
  only `E2E_DRAIN=1 pytest tests/e2e/test_node_drain.py`.
- `docs/operations.md`: the drain lane is no longer opt-in-only; note it runs
  in CI.
- `CHANGELOG.md`.

**Scope grew on first execution** (as with the upgrade suite in #145): the
lane's first local run exposed a real product bug — since #98's hard cutover
to per-session runtime keys, the broker's WS terminal relay never authorized
its upstream connect (401 from every hardened runtime; `ensure_pty` still sent
the removed static key). Fixed under #147 in the same change: the relay fetches
the sandbox's `owui-runtime-key-*` secret (fail-closed), authorizes
`ensure_pty`, and carries `Authorization: Bearer <key>` on the upgrade request;
`ws_relay.rs` captures and asserts the header.

No spec deltas — the terminal relay contract is unchanged from the client's
perspective (it was simply broken against hardened runtimes).

## Impact

One more runc KIND lane (~10 min, parallel to the other matrix arms). A real
`kubectl drain` (multi-node + RWX) remains future work, documented as such.
