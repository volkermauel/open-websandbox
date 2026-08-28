# Proposal: WS terminal traffic refreshes broker-last-used

## Why

Production trace (chat `8606eb0b`, issue #158): the terminal WS relay started at
09:03:58, the last HTTP resolve touched `broker-last-used` at 09:06:12, and the
leader reaper **parked the actively-used sandbox at 09:08:31 (`idle=139s`)** —
deleting the pod and killing the relay mid-session. Only HTTP resolves refresh
the annotation; a long-lived terminal session (even one being typed in) never
does, so every terminal-only chat parks after `parkIdleSeconds` (default 120s).

FileNav's `/files` polling compensates *only* while a files pane is open.

## What Changes

- The WS relay (`rust/broker/src/terminal.rs`) refreshes `broker-last-used` on
  relayed frames, throttled by `BROKER_WS_TOUCH_INTERVAL_SECONDS`
  (default `45`, well under the 120s park idle; `0` disables). Both directions
  count as activity — user typing **and** a running command's output the user
  is watching. Each pump keeps its own throttle window; at most ~2 annotation
  writes per interval per session, best-effort (errors logged, never fatal).
- New metric `owui_broker_ws_touches_total{direction=client|upstream}` so the
  behavior is observable in production.
- Boot warning when `wsTouchIntervalSeconds >= parkIdleSeconds` (misconfigured:
  touches would never win the race).
- Chart/base env, values + schema; docs (operations.md, deploy.md); CHANGELOG.

## Impact

- Affected: broker `terminal.rs`, `metrics.rs`, `shared/config.rs`, chart
  `templates/broker.yaml` + `deploy/base/broker.yaml`, `values.yaml`,
  `values.schema.json`, docs. RBAC unchanged (annotation patch is already
  allowed — the reaper and resolve paths write the same annotation).
- Risks: none new — the touch is the same write `resolve_sandbox` already
  performs; the throttle bounds the write rate at 2/interval/session.
