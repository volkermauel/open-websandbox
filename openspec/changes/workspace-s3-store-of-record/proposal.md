# Proposal: S3 store-of-record for chat workspaces (VCT scratch + retention)

## Why

Today the persistent profile makes the broker create and track **one RWX PVC
per user** (`workspace-p-<hash>`), with per-chat subPaths, park/resume on that
PVC, and S3 tiering as an add-on. Operational reality: those PVCs accumulate —
hundreds of long-lived PV/PVC objects whose cleanup depends on our reaper
being right, on RWX scheduling, and on nobody misconfiguring a reclaim policy.

Upstream research (`../agent-sandbox-1.0-upgrade/research.md` §3.3) showed the
missing primitive: **pool-created Sandboxes instantiate the template's
`volumeClaimTemplates`**, so a warm-pool member carries its own PVC that (a)
exists before the session (warm claiming works), (b) follows the chat through
adoption, and (c) is **garbage-collected automatically** when the Sandbox dies
(ownerRef chain) — no broker cleanup, no orphaned PVs.

Design principle (owner decision, 2026-10-20): **the sandbox is ephemeral;
the content is not.** S3 becomes the store of record for chat workspaces, with
bucket lifecycle/retention as the cleanup mechanism. Kubernetes holds only
per-sandbox scratch PVCs that die with their Sandbox.

## What Changes

### Storage model (persistent profile redefined)

- **Scratch**: every sandbox (warm member or cold) mounts a per-sandbox VCT
  PVC (`workspace`, CephFS RWO, default 5Gi — tunable). GC'd with the
  Sandbox. RWO now suffices everywhere (no shared RWX).
- **Store of record**: S3, key layout `users/<uhash>/chats/<chash>/latest.zst`
  (plus optional dated snapshots). RustFS/S3 lifecycle rules expire chat
  prefixes after a configurable idle age — cleanup is a bucket policy, not a
  k8s controller.
- **Suspend** (idle > `parkIdleSeconds`): snapshot `/workspace` → S3 upload
  (existing runtime-API seam, `s3.rs`) → delete the claim → pod + PVC GC'd.
  Nothing k8s-side remains but the Sandbox CR? No — the claim deletion
  cascades all three; only S3 objects persist.
- **Resume/revisit** (resolve for a chat with an S3 snapshot): claim binds a
  warm member → restore snapshot into the fresh PVC (existing
  restore-on-resume path, gated before readiness) → serve. Restore latency is
  the known hybrid-tiering cost, size-dependent.
- **Reap** (idle > `reapSeconds`): same archive-then-release as suspend —
  after this, only S3 (now subject to retention) remains.

### Retired

- Broker-managed per-user PVCs (`ensure_workspace_pvc`, per-user PVC prefix,
  `apply_persistent_volume` subPath injection) — deleted.
- `shared-subpath` mode and the shared RWX PVC — deleted (one storage story).
- Draft adoption as a PVC Job move (`move_workspace_dir`) — replaced by an
  S3 server-side copy `draft → chat` prefix (RustFS CopyObject).
- RWX StorageClass as a platform prerequisite — RWO CephFS (or any dynamic
  provisioner) suffices for scratch.

### Unchanged

- Warm-pool claiming, pool-key auth, pod-direct readiness (v0.2.1 work).
- Ephemeral profile: emptyDir scratch, no persistence — remains for
  disposable use.
- Per-session runtime keys, RBAC, netpol (S3 egress already allowed: public
  443; S3 creds stay broker/runtime-scoped as today, #48).

### Migration (existing deployments)

- One-shot, flag-gated migration: for each existing per-user PVC, a Job tars
  each chat subPath → uploads to its S3 chat key (same archive format as
  suspend). Verification (size/hash) before the PVC is deleted. No chat
  downtime: migrate per chat lazily on first resolve after upgrade (fast
  path), with a bulk sweep for idle chats.
- `BROKER_PERSISTENT_MODE` gains/keeps values for the transition
  (`per-user-pvc` legacy vs new `s3-record` default), removed after one
  minor.

## What does NOT change

- Warm pool: members are replaced/recycled as today; each new member gets a
  fresh empty PVC from the VCT — content never lives in member identity.
- KIND e2e pool stays disabled; new e2e lane for archive/restore with the
  RustFS fixture (pattern exists: `e2e-s3`).
- ResourceQuota: PVC count math changes (per-sandbox scratch only) — quota
  docs updated.

## Non-goals

- Sandbox-level S3 hooks (preStop/initContainers) — rejected: creds in
  untrusted pods or netpol holes; broker seam already covers it (see #205
  analysis comment).
- Per-chat StorageClass/size overrides via claims (template-identical VCTs
  are the only warm-compatible shape).
- Keeping `shared-subpath` in any form.

MCP/frontend: none (storage model internal).

## Impact

- `rust/broker/`: resolve (storage-model branches), reaper (suspend =
  archive+release), s3.rs (keys + lifecycle config + draft copy), store.rs
  (drop per-user PVC APIs), sandbox.rs (drop subPath injection)
- `rust/shared/src/config.rs`: persistent-mode redefinition, retention knobs
  (`s3.retentionDays` exists), scratch VCT values
- chart: pool template VCT render (values-gated), quota defaults
- e2e: new `e2e-s3-record` lane (archive → release → restore), migration
  test
- docs: deploy/operations/architecture rewrite of the storage chapter
