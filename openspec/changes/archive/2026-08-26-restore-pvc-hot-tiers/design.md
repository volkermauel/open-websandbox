# Design — PVC hot tiers with per-chat isolation

## Context (recovered Python-era semantics)

- `94bc2b7` (pre-squash): per-USER `SandboxClaim` with
  `volumeClaimTemplates: [workspace]` (RWX, cephfs); the controller merges the VCT
  into the pod replacing the template's same-named emptyDir. All chats of a user
  shared one sandbox and one whole-PVC `/workspace` (no chat isolation).
- `572d5fb` (pre-squash): `shared-subpath` — direct per-user `Sandbox`, cloned
  podTemplate, `workspace` volume repointed at the static `workspace-shared` PVC,
  mount `subPath: users/<user_id>/` (cross-user isolation only).

## New invariants

1. **PVC granularity = user. subPath granularity = chat.** A chat's terminal can
   only ever see (and delete) its own chat directory. PVC-per-user keeps quota and
   reclaim per user; per-chat subPaths keep chats mutually blind.
2. **Deterministic, hash-only paths.** PVC names and subPath components use
   `sha256(...)[:12]` hex of broker-controlled inputs only — raw `X-User-Id` /
   `X-Session-Id` never reach a volume name or a path (no traversal, no invalid
   path chars), and any broker replica computes the same layout:
   - `per-user-pvc`: PVC `workspace-p-<sha256(user)>`, subPath `chats/<sha256(user/session)>`
   - `shared-subpath`: PVC `workspace-shared` (chart-rendered), subPath `users/<sha256(user)>/chats/<sha256(user/session)>`
   - Sandbox names unchanged (`owui-c-<sha256(user/session)>` — pre-existing).
3. **Mode exclusivity.** `s3-tiered` ⟺ `broker.s3.enabled`; everything else fails
   closed at boot. S3 restore/offload must never fight a PVC-backed workspace.
4. **No init containers.** kubelet creates missing subPath directories; the pod
   `securityContext.fsGroup: 1000` (already in the SandboxTemplate) makes them
   group-writable by the sandbox uid. This is exactly the mechanism the Python
   broker relied on; nothing new is introduced.

## Mechanics (Rust)

- `sandbox.rs::apply_persistent_volume(pod_template, claim, sub_path)` — pure JSON
  surgery: `volumes[name=workspace]` → `{persistentVolumeClaim: {claimName}}`,
  every container's `volumeMounts[name=workspace]` gains `subPath`. Errors if the
  template has no `workspace` volume/mount (template contract).
- `store.rs::ensure_workspace_pvc` — kube `PersistentVolumeClaim`
  create-if-missing (409 → get), name/accessModes/storage/storageClassName from
  config; a `shared` mode existence check reuses the same API get. In-memory test
  store records the PVC.
- `resolve.rs` — on the persistent path, before `build_sandbox`: compute
  `(claim_name, sub_path)` from mode, `ensure_workspace_pvc`, apply surgery, stamp
  `broker-persistent-mode` on the Sandbox labels. Ephemeral and s3-tiered paths
  unchanged.

## Reaping

Persistent non-S3 sandboxes park (`operatingMode: Suspended`) at
`parkIdleSeconds` — pod gone, PVC untouched, resume is a pod recreate over the
same subPath. Reap at `reapSeconds` deletes the Sandbox; PVC + chat dir remain
(deterministic names), so a returning chat restores transparently. Chat dirs of
never-returning chats are orphaned on the volume — same trade-off as the Python
broker ("orphaned but harmless"); a future change may add chat-dir GC.

## KIND/e2e

local-path is RWO: single-node KIND keeps all chat pods of a user on one node, so
both modes are exercisable with `accessModes: [ReadWriteOnce]`. Prod keeps RWX
(cephfs). The e2e PVC job is runc-only (storage is orthogonal to the runtime
class; the gVisor matrix leg stays on the ephemeral profile).
