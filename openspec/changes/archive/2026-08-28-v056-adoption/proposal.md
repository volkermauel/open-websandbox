# Proposal: v0.5.6 adoption (retroactive record)

## Why

Follow-up to `agent-sandbox-v0.5.6` (#154): adopt the v0.5.4–v0.5.6
capabilities our own components can use. (This change doc was written at
archive time; the work shipped as PR #156 / issue #155.)

## What Changed

- Broker `wait_for_ready`: the timeout 503 carries a one-line digest of the
  sandbox's last-seen status conditions (phase, type=status reason=message,
  clipped). The v0.5.6 controller mirrors `PodScheduled` into
  `Sandbox.status.conditions`, so `Unschedulable`/`SchedulingGated` and
  suspension reasons surface directly in the API error users hit.
- sandbox-router: `--max-request-body-bytes=268435456` (256 MiB, aligned with
  the broker's `MAX_FORWARD_BODY`) in chart + deploy/base — the Go router
  defaults to 0/unlimited and direct-to-router clients bypass the broker's cap.
- Deferred (documented): router authz modes (no token-presenting clients),
  controller warm-pool grace flags (would break verbatim vendoring),
  WarmPool `observedGeneration` (no broker consumer).

## Impact

Additive diagnostics + a hardening cap; no API/CRD changes. Three new broker
unit tests; chart/base router args verified in parity.
