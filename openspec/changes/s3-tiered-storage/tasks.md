# Tasks

## Phase 1 — OpenSpec change

- [x] `openspec/changes/s3-tiered-storage/`: proposal + spec + tasks encoding D1–D9 + R1/R2.
- [x] Commit.

## Phase 2 — runtime snapshot/restore endpoints

- [x] `runtime/server.py`: `GET /snapshot` (stream `tar | zstd`, size pre-check 413) +
  `PUT /restore` (stream `zstd -d | tar -x`, size cap 413), both `_auth_runtime`-gated.
- [x] `runtime/Dockerfile`: add `zstd` (the pipeline binary).
- [x] `tests/unit/runtime/test_snapshot_restore.py`: round-trip on tmp workspace, size
  fail-on-exceed, auth gating.

## Phase 3 — broker S3 client + orchestration

- [x] `broker/main.py`: S3 config + soft `aioboto3` import + lazy client; `_s3_object_key`,
  `_s3_prefix`, `_offload_to_s3` (multipart streaming, SSE-S3), `_restore_from_s3`
  (streaming); `_create_sandbox` s3-tiered emptyDir/workspace override + `broker-persistent-mode`
  label.
- [x] Hook `_offload_to_s3` (retry+backoff, D7 keep-alive) into the reaper's s3-tiered reap.
- [x] Hook `_restore_from_s3` into `resolve_sandbox` (D4 sync; fail-on-error; no-op first
  creation).
- [x] `_validate_config`: refuse `s3-tiered` without `s3.enabled`.

## Phase 4 — periodic-sync scheduler (R1)

- [x] `broker/main.py`: `_periodic_sync_loop` + `_periodic_sync_once`; start/stop in
  `_apply_leadership` (leader-gated, only when `S3_ENABLED`).

## Phase 5 — per-session retention (R2 / D5)

- [x] `broker/main.py`: keep-latest (delete prior objects under prefix) + object
  `Expires`/metadata at offload.
- [x] `docs/s3-tiered-storage.md`: documented the recommended bucket lifecycle rule.

## Phase 6 — chart wiring

- [x] `chart/values.yaml` + `values.schema.json`: `broker.s3.*`, `persistentMode` enum
  `s3-tiered` (closed enum, `s3.*` required).
- [x] `chart/templates/broker.yaml`: projected `/etc/s3-creds` Secret volume + `BROKER_S3_*`
  env, gated on `broker.s3.enabled`. creds Secret is bring-your-own (referenced, not created).
- [x] Runtime `MAX_WORKSPACE_BYTES` env wired from `broker.s3.sizeLimit` (sizeBytes helper).
- [x] `broker/requirements.in`: add `aioboto3==15.5.0`; regenerate the hashed lock.

## Phase 7 — tests

- [x] `tests/unit/broker/test_s3_tiered.py`: fake in-memory S3 client; offload, restore,
  periodic-sync, retention-expiry, D7 retry/keep-alive, D4 sync/fail-resume, s3-tiered
  sandbox create (emptyDir override + label), object-key format, boot guards (17 tests).
- [x] Keep existing broker/runtime tests green (additive only): full suite 337 passed.

## Phase 8 — verify + ship

- [x] `helm lint` + `helm template` (default clean / byte-identical to main modulo the random
  shared-secret; s3-on renders creds + variant) + `ruff check` + `python3 -m pytest tests/unit -q`
  green.
- [x] Vendored controller/CRDs/upstream byte-for-byte unchanged; PVC modes untouched.
- [ ] Open PR; CI default e2e green (s3 off by default). MinIO-in-cluster e2e = scoped follow-up.
