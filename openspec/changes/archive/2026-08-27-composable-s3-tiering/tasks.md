# Tasks

## 1. Rust shared config

- [x] 1.1 Replace `PersistentMode::S3Tiered` with `EmptyDir` (`empty-dir`); clear
       FromStr error naming the migration; update enum unit tests.
- [x] 1.2 Boot validation: `empty-dir && !s3_enabled` fatal; all PVC × s3 combos
       valid; drop the old `s3-tiered ⟺ s3_enabled` rule; tests for every combo.

## 2. Rust broker

- [x] 2.1 `reaper.rs::decide`: never-park/reap-at-idle branch keyed on
       `EmptyDir` hot tier (not `s3_enabled`); PVC modes park/reap normally with
       or without s3; retest matrix incl. `pvc + s3 parks`, `pvc + s3 reaps at
       reapSeconds`, `empty_dir never parks`.
- [x] 2.2 `s3.rs::offload_on_reap`: resume-if-Suspended (patch Running, poll
       Ready/pod-ip bounded by claim timeout) before snapshotting; tests with
       stub store (suspended → resumed → offloaded; resume failure keeps alive).
- [x] 2.3 `s3.rs`: purge step after fully-successful offload on PVC-backed
       sandboxes (POST /execute `find`-delete of the chat dir via runtime);
       non-fatal on failure; tests incl. purge-not-called-for-empty-dir.
- [x] 2.4 `resolve.rs`: handle `restored: false` from the runtime (log hot-tier
       hit, proceed); outcome enum + tests.
- [x] 2.5 `cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
       cargo test --workspace` green.

## 3. Rust runtime

- [x] 3.1 `snapshot.rs::restore`: restore-if-empty — non-empty workspace →
       `200 {restored: false}`; extend `RestoreResponse`; openapi schema;
       unit tests (non-empty skip, empty proceeds, response shape).

## 4. Chart + base

- [x] 4.1 `values.yaml` + `values.schema.json`: enum
       `per-user-pvc|shared-subpath|empty-dir`; s3.enabled documented as
       composable cold tier (required by empty-dir).
- [x] 4.2 `values-kind-s3.yaml` → `persistentMode: empty-dir`.
- [x] 4.3 New `values-kind-pvc-s3.yaml` (per-user-pvc + MinIO s3, short park/
       reap timers for determinism).
- [x] 4.4 `helm lint` + render all profiles; deploy/base parity.

## 5. e2e + CI

- [x] 5.1 `conftest.py`: `require_pvc_s3` fixture (E2E_PVC_S3=1).
- [x] 5.2 New `test_pvc_s3_tiering.py`: park-resume no-clobber; reap → MinIO
       object + chat dir purged from PVC (debug pod mount); re-resolve restores
       from S3.
- [x] 5.3 CI: `e2e-pvc` matrix lane `pvc-s3` (MinIO fixture + values-kind-pvc-s3)
       running `test_pvc_persistence.py` + `test_s3_tiered.py` + the new file.
- [x] 5.4 Local KIND run of all three PVC lanes (pvc, pvc-shared, pvc-s3).

## 6. Docs

- [x] 6.1 deploy.md: hot×cold tier matrix, lifecycle per hot tier, env notes.
- [x] 6.2 architecture.md (state diagram + mode table), operations.md (backup
       in hybrid mode), CHANGELOG.
- [x] 6.3 `mkdocs build --strict` green.
