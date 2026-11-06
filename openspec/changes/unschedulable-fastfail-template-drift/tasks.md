# Tasks: unschedulable fast-fail + template-drift self-heal

## 1. Fast-fail on terminal unschedulability

- [x] 1.1 `resolve.rs`: `pod_unschedulable_message` /
      `sandbox_unschedulable_message` — `PodScheduled=False
      reason=Unschedulable` detection on the Pod object and the Sandbox-status
      mirror (`SchedulingGated` excluded)
- [x] 1.2 `resolve.rs`: `wait_for_ready` returns
      `ApiError::ServiceUnavailable("sandbox <name> unschedulable: <message>")`
      immediately; WARN with sandbox + reason + condition; condition message
      clipped to one line
- [x] 1.3 Claim path inherits the fast-fail (shared `wait_for_ready` on the
      bound sandbox); claim-bind timeout counted as `reason=timeout`
- [x] 1.4 `metrics.rs`: `open_websandbox_broker_sandbox_not_ready_total`
      counter, fixed reasons `unschedulable|timeout|other`, seeded at boot

## 2. Template-drift self-heal (idle only)

- [x] 2.1 `sandbox.rs`: `template_hash` (sha256 over canonical JSON of
      `spec.podTemplate`, key-order-free) + `owui.io/template-hash` stamp in
      `build_sandbox` (hash of the PRISTINE template, before clone mutation)
- [x] 2.2 `resolve.rs`: create branch extracted into
      `create_sandbox_from_template(store, cfg, s3_restore, …)` so resolve AND
      the reaper share the exact cold-create path (PVC ensure, draft-adoption
      plan + pending marker, runtime key, finalizer, mode label, hash stamp)
- [x] 2.3 `reaper.rs`: idle clock extracted into `idle_secs` (last-used +
      #209 s3-record creation fallback) and reused by `recycle_eligible` —
      no second notion of idle; guards: not Suspended, not terminating, 120s
      `broker-recycled-at` grace window
- [x] 2.4 `reaper.rs` tick: one live-template hash read per tick, drift +
      eligibility ⇒ offload (D7, failure defers) → delete → recreate →
      stamp; `ReaperStats.recycled` + tick-log field
- [x] 2.5 `resolve.rs`: `recycle_drifted_idle` guard on the existing-sandbox
      path (drift + eligible ⇒ delete + fall into the shared create path),
      `recycled sandbox <name>: template <old> -> <new>` log, unreadable
      template ⇒ debug/warn + keep serving

## 3. Tests

- [x] 3.1 Unschedulable pod condition ⇒ immediate 503 carrying the condition
      message, elapsed far under the claim timeout (both sources: Pod object
      and Sandbox-status mirror)
- [x] 3.2 `SchedulingGated` ⇒ still waits, then the normal timeout 503
- [x] 3.3 Hash stamping: created Sandbox annotation matches
      `template_hash(&template)`; hash canonical/stable; None without a
      podTemplate
- [x] 3.4 Drift + idle ⇒ recycled (delete + create observed: new hash, grace
      stamp, full broker field set); drift + active ⇒ not recycled; parked ⇒
      never; legacy (no stamp) ⇒ never; unreadable template ⇒ keep serving
- [x] 3.5 Reaper recycles an idle drifted sandbox; skips an active one; grace
      window blocks an immediate repeat; `recycle_eligible` unit tests
      (parked/terminating/grace/untracked)
- [x] 3.6 Counter: fixed reason series materialise (`unschedulable`,
      `timeout`, `other`), catalogue test extended
