# Restore PVC hot tiers (per-user-pvc, shared-subpath) with per-chat isolation

## Why

The Rust rewrite never ported the two PVC-backed hot tiers from the Python broker.
`broker.persistentMode` (`per-user-pvc` — the chart default — and `shared-subpath`)
is advertised by the chart, RBAC, and docs, but the Rust broker ignores it: every
"persistent" sandbox runs on an emptyDir and its data is destroyed on park/reap.
Silent data loss under the default install (#140).

## What Changes

- `shared::BrokerConfig` gains `PersistentMode` (`per-user-pvc | shared-subpath |
  s3-tiered`, default `per-user-pvc`) plus the knobs it needs:
  `persistent_storage` (exists as env, now read), `persistent_storage_class`,
  `persistent_access_modes`, `shared_pvc_name`, `per_user_pvc_prefix`.
  Boot fails closed on an unknown mode and on inconsistent S3 combinations
  (`s3-tiered` ⟺ `broker.s3.enabled`).
- `rust/broker/src/sandbox.rs` gains the pure pod-template surgery
  `apply_persistent_volume`: repoint the `workspace` volume at a PVC and set a
  per-chat `subPath` on its mount (cloned pod template, same mechanics as the
  Python broker).
- `SandboxStore::ensure_workspace_pvc` creates (create-if-exists, 409-tolerant)
  the per-user PVC in `per-user-pvc` mode; `shared-subpath` checks the
  chart-rendered shared PVC exists and fails with a clear error otherwise.
- `resolve.rs` applies the surgery on the persistent path only. Sandbox names are
  unchanged (`owui-c-<sha256(user/session)[:12]>` — already per chat).
- Per-chat isolation (the project's stated intent, `openspec/config.yaml`):
  - `per-user-pvc`: PVC `workspace-p-<sha256(user)[:12]>` per **user**
    (quota/economics), mount subPath `chats/<sha256(user/session)[:12]>` per **chat**.
  - `shared-subpath`: PVC `workspace-shared` shared by everyone, mount subPath
    `users/<sha256(user)[:12]>/chats/<sha256(user/session)[:12]>`.
  - Hash-only path components (no raw user/session input → no traversal).
  - kubelet auto-creates missing subPath dirs; the pod's `fsGroup: 1000` grants
    the sandbox uid write access (same mechanics the Python broker relied on).
- Chart: `shared-pvc.yaml` renders **only** in `shared-subpath` mode (kills the
  forever-Pending RWX PVC in ephemeral lanes); `sharedPvc.accessModes` +
  `broker.persistentAccessModes`/`persistentStorageClass`/`perUserPvcPrefix`
  values; env `BROKER_PERSISTENT_STORAGE_CLASS`, `BROKER_PERSISTENT_ACCESS_MODES`,
  `BROKER_SHARED_PVC`, `BROKER_PER_USER_PVC_PREFIX` added; `deploy/base/` synced.
- Reaper unchanged: persistent non-S3 sandboxes park at `parkIdleSeconds` and are
  reaped at `reapSeconds`; the PVC + chat subPath survive reap, so a later resolve
  of the same chat recovers its data (orphaned chat dirs of never-returning chats
  stay on the volume — documented, same as the Python broker).
- e2e: `values-kind-pvc.yaml` (+ shared variant) and `tests/e2e/test_pvc_persistence.py`
  (E2E_PVC=1): persistence across sandbox deletion/re-resolve, cross-chat
  isolation (chat B cannot see or delete chat A's files), PVC naming.
- Docs: deploy.md mode table + env table (also fixes the `profile.*` → `broker.*`
  path drift), architecture.md, operations.md, CHANGELOG, AGENTS.md gotcha.

## Impact

- Affected specs: persistent-workspace (no baseline spec directory exists yet; this
  change carries the spec delta inline in `design.md`).
- Risks: RWO-only clusters (KIND local-path) restrict per-user PVCs to same-node
  multi-mount; acceptable for e2e (single-node KIND), documented for prod (RWX).
