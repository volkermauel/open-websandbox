# Proposal: unschedulable fast-fail + template-drift self-heal

## Why

Live incident, 2026-11-06 (MCO rolling upgrade, `okd-w1` cordoned):

1. **Terminal unschedulability looked "slow" instead of failing.** The
   s3/persistent `SandboxTemplate` pinned `kubernetes.io/hostname=okd-w1`, so
   every persistent sandbox hit `FailedScheduling` (`PodScheduled=False
   reason=Unschedulable`, `0/11 nodes`). The broker's readiness poll kept
   polling until `claim_timeout_seconds` (60s) and then returned a generic
   503 — Open Web UI showed "extremely slow" instead of failing with the
   reason the conditions already carried.
2. **Template fixes never reach existing sandboxes.** Each `Sandbox` CR
   stores a COPY of the template `podTemplate` at creation
   (`build_sandbox`), so fixing the template never repairs sandboxes created
   from the broken revision — they stayed unschedulable until deleted by
   hand.

## What Changes

1. **Fast-fail on terminal unschedulability** (`broker/resolve`):
   `wait_for_ready` treats `PodScheduled=False reason=Unschedulable` — from
   the Pod object or the Sandbox-status mirror — as terminal and returns 503
   immediately with the condition message
   (`sandbox <name> unschedulable: <message>`), a WARN line (name + reason +
   condition), and a new counter
   `open_websandbox_broker_sandbox_not_ready_total{reason=unschedulable|timeout|other}`.
   The same fast-fail applies on the claim path (shared `wait_for_ready` on
   the bound sandbox). `SchedulingGated` is deliberately NOT terminal — gates
   can lift, so it keeps the wait/timeout contract. The timeout path keeps
   its condition digest and gains `reason=timeout` on the counter.
2. **Template-drift self-heal, idle only** (`broker/sandbox-lifecycle`):
   - Stamp: broker-created Sandboxes carry
     `owui.io/template-hash` = sha256 over canonical JSON of the template
     fields cloned into the Sandbox (today: `spec.podTemplate` — the only
     cloned field on `SandboxTemplateSpec`; env/claim policies would join the
     hash when the CRD gains them). Hashed BEFORE the clone is mutated
     (profile label, runtime-key volume, PVC surgery).
   - Detect: on resolve, and once per reaper tick, compare the live
     `SandboxTemplate` hash against the Sandbox stamp.
   - Recycle (idle only): delete the drifted Sandbox and recreate it through
     the shared cold-create path (`create_sandbox_from_template`). Eligibility
     reuses the reaper's exact idle predicate (`decide()` — `broker-last-used`
     freshness), with explicit guards: never a parked (`Suspended`) sandbox,
     never a terminating one, never within a 120s anti-loop grace window
     (`broker-recycled-at` stamp), never a sandbox without a stamp
     (legacy/controller-built), and a failed pre-delete offload (D7) defers
     the recycle. The reaper's tick is the main path (idle sandboxes get no
     requests); resolve recycles opportunistically when it finds an idle
     drifted sandbox. Log: `recycled sandbox <name>: template <old> -> <new>`;
     reaper stats gain `recycled`.

## Non-goals

- Recreating claimed (warm-pool) Sandboxes on drift: their template comes
  from the pool template, they carry no stamp, and the existing idle reap +
  pool replenishment already replaces them with the current template.
- Backfilling stamps onto pre-existing (legacy) CRs — they keep serving until
  naturally recreated.
- Making unschedulability recoverable mid-wait (e.g. uncordon detection): the
  scheduler reason is treated as terminal per ops decision; a changed
  cluster state surfaces on the user's next resolve (cold create).
