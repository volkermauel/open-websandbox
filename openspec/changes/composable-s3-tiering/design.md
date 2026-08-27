# Design: Composable S3 tiering

## Context / Goals

- Hot tier (`broker.persistentMode`): `per-user-pvc` | `shared-subpath` | `empty-dir`.
- Cold tier (`broker.s3.enabled`): orthogonal; composes with every hot tier.
- Persistent lifecycle keys off the hot tier; offload/restore key off the cold tier.

## Detailed Design

### 1. `shared/config.rs`

- `PersistentMode` loses `S3Tiered`, gains `EmptyDir`. `FromStr` error message
  names the replacement (`s3-tiered` → set `broker.s3.enabled` and pick a hot
  tier; `empty-dir` requires it).
- New helper `BrokerConfig::cold_tier_enabled(&self) -> bool { self.s3_enabled }`
  (single point of truth; keeps call sites honest).
- Boot validation:
  - `persistent_mode == EmptyDir && !s3_enabled` → hard error (the #140
    silent-data-loss class).
  - `persistent_mode != EmptyDir` + `s3_enabled` → valid (the new composable
    combinations).

### 2. `rust/broker/src/reaper.rs` — `decide`

Persistent arm splits on the hot tier instead of `s3_enabled`:

| hot tier | park | reap |
|---|---|---|
| `EmptyDir` | never (pod must live to snapshot; with s3 → reap = offload+delete; without s3 → unreachable: boot rejects the combo) | `idle_secs > idle_ttl_seconds` |
| PVC modes | `idle_secs > park_idle_seconds && !Suspended` (unchanged, s3-agnostic) | `idle_secs > reap_seconds` (unchanged, s3-agnostic) |

Ephemeral arm unchanged.

### 3. `rust/broker/src/s3.rs` — `S3Offload`

- `offload_on_reap(sandbox)`: profile guard unchanged; **new**: if the sandbox
  is `Suspended`, resume it first (patch `operatingMode: Running` via the store,
  poll Ready + pod IP, bounded by `claim_timeout_seconds`), offload, then
  continue (the reaper deletes the Sandbox right after — the brief pod is the
  price of a clean offload; a resume failure aborts the offload so the sandbox
  stays alive for the next tick).
- New `purge_workspace(sandbox, pod_ip, user, session)`: after a successful
  offload on a PVC-backed sandbox, execute a shell `rm -rf` of the chat
  sub-directory **through the runtime** (`POST /execute`), not from the broker —
  the runtime's `safe_path` is the confinement boundary. Path is the
  broker-known `subPath` for the sandbox's mode (never user input). Failure is
  logged + non-fatal (object already durable; next reap converges).
- `restore_on_resume` unchanged (the runtime gate below makes it safe).

### 4. `rust/runtime/src/snapshot.rs` — restore-if-empty

- `PUT /restore` pre-check: if the workspace root contains anything, return
  `200 {restored: false, bytes: 0}` without touching the body. The broker
  treats `false` as a hot-tier hit (no error). emptyDir semantics unchanged —
  a fresh pod's workspace is empty, so restore proceeds.
- Rationale: with PVC × S3, resolve fires restore on every resume; only the
  runtime knows whether the mount is hot.

### 5. Chart

- `values.yaml`/schema: `persistentMode` enum `per-user-pvc | shared-subpath |
  empty-dir`; `broker.s3.enabled` documented as the orthogonal cold tier.
- `values-kind-s3.yaml`: `persistentMode: empty-dir` (+ existing s3 block).
- New `values-kind-pvc-s3.yaml`: per-user-pvc + MinIO s3 block (hybrid lane).

### 6. e2e + CI

- `conftest.py`: `require_pvc_s3` fixture (E2E_PVC_S3=1).
- New `tests/e2e/test_pvc_s3_tiering.py`:
  1. park-resume serves PVC data (no S3 clobber — restore-if-empty honored);
  2. reap offloads to MinIO AND the chat dir is gone from the PVC (debug pod
     mounts the PVC and asserts);
  3. re-resolve after reap restores from S3.
- `test_s3_tiered.py` runs unchanged against the PVC lane (agnosticism proof).
- CI: extend `e2e-pvc` matrix with a third lane (`pvc-s3`) reusing the MinIO
  fixture deploy from `e2e-s3`.

### 7. Docs

deploy.md mode table gains the "S3 cold tier" column (composes ✓ everywhere;
required for `empty-dir`), env reference + lifecycle table updated,
operations.md backup section notes hybrid tiering, architecture.md state
diagram updated, CHANGELOG.

## Risks / Trade-offs

- **Reap-of-parked costs a pod**: unavoidable — the snapshot lives in the
  runtime. Bounded by claim timeout; only at final reap (once per chat).
- **Purge is best-effort**: a failed purge leaks a stale dir until the next
  reap; data-safe (S3 copy is authoritative for that point in time).
- **`s3-tiered` removed**: clear boot error names the migration. Never shipped
  in a release (v0.1.0 ignored it; #141 landed today).
- **restore-if-empty changes runtime semantics**: only observable in PVC × S3
  (new combination); emptyDir behavior identical.

## Migration Plan

- Chart default unchanged (`per-user-pvc`, s3 off). `s3-tiered` users (none in
  the wild pre-release) get a boot error with exact replacement values.
