# Tasks: terminating-finalizer-release-legacy-drift

## 1. No-pod terminating release (BUG 1)

- [x] 1.1 `reaper.rs`: `handle_terminating` releases the finalizer WITHOUT
      an offload when the sandbox has no pod IP and is not `Suspended`
      (WARN names sandbox + reason); offload attempted only when a pod IP
      exists or the sandbox is Suspended
- [x] 1.2 `reaper.rs`: `ReaperStats.termination_released_no_offload` + tick
      log field
- [x] 1.3 `metrics.rs`: `open_websandbox_broker_termination_released_without_offload_total`
      counter (described + seeded at boot, catalogue test extended)
- [x] 1.4 D7 distinction preserved: pod IP + failing offload ⇒ held
      (test), Suspended + no IP ⇒ held (test), no pod ⇒ released with
      counters/log assertions (tests)

## 2. Legacy drift detection without the hash stamp (BUG 2)

- [x] 2.1 `sandbox.rs`: `TEMPLATE_REF_ANNOTATION_KEY` constant;
      `stamped_template_hash` (moved out of the reaper's private
      `template_hash_of`); `template_name_of` resolution order
      (owui.io/template-name → agents.x-k8s.io/sandbox-template-ref →
      caller's base fallback)
- [x] 2.2 `sandbox.rs`: `template_drifted(sbx, live)` — hash path first;
      legacy path compares actual `spec.podTemplate` vs the live
      template's by canonical-JSON equality; every incomparable case
      (no stamp + no copied podTemplate, template without podTemplate)
      returns false (never recycle)
- [x] 2.3 `resolve.rs`: `recycle_drifted_idle` uses the shared helpers —
      resolution order + legacy comparison on the resolve path; idle guard,
      grace window and pre-delete offload (D7) unchanged
- [x] 2.4 `reaper.rs`: drift branch caches live `SandboxTemplate`s per
      distinct name (base seeded once per tick) and decides via
      `template_drifted`; 2-per-tick cap unchanged
- [x] 2.5 Tests: legacy equal ⇒ kept, stale + idle ⇒ recycled (fresh hash
      stamp), unreadable template ⇒ untouched, controller-ref custom
      template judged against itself; reaper-path legacy recycle / kept /
      no-podTemplate; pre-existing drift/reaper/fast-fail suites pass
      unchanged

## 3. Gates

- [x] 3.1 `cargo fmt --check`
- [x] 3.2 `cargo clippy -p broker --all-targets -- -D warnings -A unknown-lints`
- [x] 3.3 `OWUI_WS_LIVE=1 cargo test -p broker` (233 tests green)
