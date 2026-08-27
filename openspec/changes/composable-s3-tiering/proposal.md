# Proposal: Composable S3 tiering (cold tier independent of hot tier)

## Why

`broker.persistentMode: s3-tiered` couples two orthogonal concerns: the **hot tier**
(what backs `/workspace` while the sandbox runs) and the **cold tier** (S3
offload-on-reap / restore-on-resume). Choosing S3 tiering forces the emptyDir hot
tier — a deployment on per-user CephFS PVCs cannot also offload idle chats to S3,
even though every mechanism involved (runtime `GET /snapshot`, S3 put,
`PUT /restore`) is already hot-tier-agnostic.

User requirement: tiering is a property independent of the underlying storage —
S3 offload must compose with `emptyDir`, `per-user-pvc`, and `shared-subpath`.

## What Changes

- `broker.persistentMode` becomes purely the **hot tier**:
  `per-user-pvc` | `shared-subpath` | `empty-dir` (replaces `s3-tiered`).
- `broker.s3.enabled` becomes purely the **cold tier**, composable with any hot
  tier. Boot validation: `empty-dir` **requires** `s3.enabled=true` (persistent
  sandboxes on emptyDir without a cold tier lose data on pod delete — the #140
  bug class). Every PVC × S3 combination is valid.
- Lifecycle (persistent profile) keys off the **hot tier**:
  - `empty-dir`: unchanged — never parks, reaps at `idleTtlSeconds` (pod must be
    alive to snapshot; data dies with the pod).
  - PVC modes: park at `parkIdleSeconds`, reap at `reapSeconds` — regardless of
    S3. With S3 on, reap **offloads first, then clears the chat directory on the
    PVC** so the hot tier actually frees space (offload = move, not copy).
- **Reap-of-parked with S3**: a parked sandbox has no pod, but the offloader
  snapshots via the runtime. The reaper now briefly resumes a Suspended sandbox
  (patch `Running`, wait Ready), offloads + purges, then deletes it. Without
  this, a parked s3-enabled sandbox could never be reaped (offload error → keep
  alive → immortal sandbox).
- **Restore becomes restore-if-empty**: the runtime `PUT /restore` no-ops with
  `200 {restored: false}` when `/workspace` is non-empty (PVC hot hit — e.g.
  park resume, or a purge that failed). Unpacking a stale S3 object over newer
  PVC data would silently regress state. emptyDir behavior is unchanged (a
  fresh pod's workspace is always empty).
- Purge failure (post-successful-offload workspace clear) is non-fatal: the data
  is durably in S3, the stale dir is served as a hot hit, and the next
  reap/resolve converges.

## e2e

New KIND profile `values-kind-pvc-s3.yaml` (per-user-pvc + in-cluster MinIO) and
CI job `e2e-pvc-s3` running `test_pvc_persistence.py` + `test_s3_tiered.py`
(unchanged — proving agnosticism) + a new `test_pvc_s3_tiering.py` proving true
tiering: park-resume serves PVC data without S3 clobber; reap offloads to MinIO
AND removes the chat dir from the PVC (verified by a debug pod mounting the
PVC); re-resolve restores from the cold tier.

## Impact

- Affected: `rust/shared` (mode enum + validation), `rust/broker` (reaper
  lifecycle, offloader resume+purge, restore outcome), `rust/runtime`
  (restore-if-empty), chart values/schema + new KIND profile, e2e suite + CI,
  docs.
- `s3-tiered` is removed with a clear error string — it was never released
  (v0.1.0's broker ignored the mode entirely; #141 shipped hours before this
  change).
