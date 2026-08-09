# Tasks

## Phase 1 — OpenSpec change

- [x] `openspec/changes/s3-tiered-storage/`: proposal + spec + tasks encoding D1–D9 + R1/R2.
- [x] Commit.

## Phase 2 — runtime snapshot/restore endpoints

- [ ] `runtime/server.py`: `GET /snapshot` (stream `tar | zstd`, size pre-check 413) +
  `PUT /restore` (stream `zstd -d | tar -x`, size cap 413), both `_auth_runtime`-gated.
- [ ] `tests/unit/runtime/test_snapshot_restore.py`: round-trip on tmp workspace, size
  fail-on-exceed, auth gating.

## Phase 3 — broker S3 client + orchestration

- [ ] `broker/main.py`: S3 config + soft `aioboto3` import + lazy client; `_s3_object_key`,
  `_s3_prefix`, `_offload_to_s3` (multipart streaming), `_restore_from_s3` (streaming);
  `_create_sandbox` s3-tiered emptyDir/workspace override + `broker-persistent-mode` label.
- [ ] Hook `_offload_to_s3` (retry+backoff, D7 keep-alive) into the reaper's s3-tiered reap.
- [ ] Hook `_restore_from_s3` into `resolve_sandbox` (D4 sync; fail-on-error; no-op first
  creation).
- [ ] `_validate_config`: refuse `s3-tiered` without `s3.enabled`.

## Phase 4 — periodic-sync scheduler (R1)

- [ ] `broker/main.py`: `_periodic_sync_loop` + `_periodic_sync_once`; start/stop in
  `_apply_leadership` (leader-gated, only when `S3_ENABLED`).

## Phase 5 — per-session retention (R2 / D5)

- [ ] `broker/main.py`: keep-latest (delete prior objects under prefix) + object
  `Expires`/metadata at offload. Document the recommended bucket lifecycle rule in
  `values.yaml`.

## Phase 6 — chart wiring

- [ ] `chart/values.yaml` + `values.schema.json`: `broker.s3.*`, `persistentMode` enum
  `s3-tiered` (closed enum, all `s3.*` required when broker object grows).
- [ ] `chart/templates/broker.yaml`: projected `/etc/s3-creds` Secret volume + `BROKER_S3_*`
  env, gated on `broker.s3.enabled`. creds Secret is bring-your-own (referenced, not created).
- [ ] Runtime `MAX_WORKSPACE_BYTES` env wired from `broker.s3.sizeLimit`.

## Phase 7 — tests

- [ ] `tests/unit/broker/test_s3_tiered.py`: fake in-memory S3 client; offload, restore,
  periodic-sync, retention-expiry, D7 retry/keep-alive, D4 sync/fail-resume, s3-tiered
  sandbox create (emptyDir override + label).
- [ ] Keep existing broker/runtime tests green (additive only).

## Phase 8 — verify + ship

- [ ] `helm lint` + `helm template` (default clean / identical; s3-on renders creds +
  variant) + `ruff check` + `python3 -m pytest tests/unit -q` green.
- [ ] commit per phase, push branch, open PR; CI default e2e green (s3 off by default).
