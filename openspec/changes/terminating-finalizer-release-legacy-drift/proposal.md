# Proposal: release pod-less terminating finalizers + legacy-sandbox drift self-heal

## Why

Live incident, 2026-11-06/07 (production cluster):

1. **Terminating sandboxes blocked forever.** Two sandboxes created before a
   template fix never got a pod (`FailedScheduling`: pinned to a cordoned
   node). They were deleted and then sat `Terminating` for 11 h: the
   `owui.dev/s3-snapshot` finalizer is held while the reaper retries the
   termination offload every tick, failing each time with
   `offload failed: no pod IP for …; cannot offload (leave for next tick)` —
   a hold that can never be satisfied, because the offload requires a serving
   pod and there is none. The broker kept answering "sandbox … unschedulable"
   until someone stripped the finalizer by hand.
2. **Pre-stamp sandboxes never self-heal.** v0.3.1's drift detection keys on
   the `owui.io/template-hash` stamp, so sandboxes created before the
   stamping existed keep their stale copied podTemplate forever — exactly the
   two sandboxes above. The controller already stamps
   `agents.x-k8s.io/sandbox-template-ref` on every sandbox, so the template
   name IS knowable without the broker's own stamp.

## What Changes

1. **No-pod terminating release** (`broker/reaper`): in
   `handle_terminating`, when a terminating sandbox carries the
   s3-snapshot finalizer but has NO pod IP (`status.pod_ip` absent — the
   exact first check `S3Offload::ensure_running_pod` makes) and is NOT
   `Suspended`, the finalizer is released without an offload: a WARN names
   the sandbox and the reason, a new counter
   `open_websandbox_broker_termination_released_without_offload_total` is
   incremented, and a new `ReaperStats.termination_released_no_offload`
   counts the tick. Release is safe by construction: the offloader would
   fail with the very same "no pod IP" error on every tick, so the hold
   could never converge; there is no runtime endpoint, hence no snapshot to
   lose. Two cases keep holding:
   - a pod IP exists but the offload call fails (D7 — transient S3 outage),
   - a `Suspended` sandbox (the offloader may still resume it — #207
     contract unchanged).
2. **Legacy drift detection without the hash stamp**
   (`broker/sandbox-lifecycle`, applied to BOTH the resolve path and the
   reaper path):
   - Template name resolution order:
     `owui.io/template-name` → `agents.x-k8s.io/sandbox-template-ref` →
     configured base template.
   - When `owui.io/template-hash` is missing, compare the sandbox's ACTUAL
     `spec.podTemplate` against the live template's `spec.podTemplate` via
     canonical-JSON equality (key-order free): equal ⇒ no drift,
     different ⇒ drifted. Never recycle when the template cannot be read
     (missing / apiserver error / no podTemplate) or when the sandbox has no
     copied podTemplate (controller-built / claim-bound).
   - The hash-stamp path is unchanged and still takes priority; the
     idle-only guard (`recycle_eligible`), the 120 s anti-loop grace and the
     2-per-tick cap are unchanged.

## Impact

- `rust/broker/src/sandbox.rs`: `TEMPLATE_REF_ANNOTATION_KEY`,
  `stamped_template_hash`, `template_name_of`, `template_drifted`.
- `rust/broker/src/reaper.rs`: `handle_terminating` no-pod release branch,
  `ReaperStats.termination_released_no_offload`, drift branch resolves the
  per-sandbox template via the new helpers and caches live templates.
- `rust/broker/src/resolve.rs`: `recycle_drifted_idle` uses the shared
  helpers (no re-implemented comparison).
- `rust/broker/src/metrics.rs`: the new frozen counter (described + seeded).
- `rust/broker/src/store.rs`: untouched. Chart/manifests: untouched.
- Tests: `tests/s3_termination_finalizer.rs` (no-pod release vs hold
  distinction, WARN capture), `tests/template_drift.rs` (legacy equal/stale/
  unreadable/controller-ref), `tests/reaper.rs` (legacy drift on the reaper
  path).
