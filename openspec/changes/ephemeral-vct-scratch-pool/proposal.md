# Proposal: ephemeral scratch PVCs via pool-template volumeClaimTemplates

## Why

Ephemeral chats today get an emptyDir `/workspace`: no durability while the
chat is alive beyond the pod, and nothing survives a node reschedule.
Meanwhile the warm pool (v0.2.1) proves claim-level customization can never
carry per-session volumes — claim `env`/`volumeClaimTemplates` hard-bypass
warm adoption (upstream `sandboxclaim_controller.go`, unchanged through
v1.0.7).

Research (`../agent-sandbox-1.0-upgrade/research.md` §3.3) found the
supported path: **pool-created Sandboxes copy the entire SandboxBlueprint —
including `volumeClaimTemplates`** — so every warm member gets its own PVC
`<template>-<sandboxName>`, created by the controller, **kept through claim
adoption** and garbage-collected with the Sandbox when the claim is reaped
(ownerRef chain: claim → Sandbox → PVC). Claim-level customization stays
empty, so warm claiming keeps working.

Result for the ephemeral profile: a per-chat CephFS-backed scratch volume
with **controller-owned lifecycle** (created on pool reconcile, deleted on
reap by kube GC) — no broker PVC code, and per-sandbox RWO instead of
shared-RWX.

The persistent profile is explicitly out of scope: durability there must
outlive Sandbox deletion, which ownerRef-GC semantics forbid. Broker-managed
per-user PVC + S3 tiering stays.

## What Changes

- Pool SandboxTemplate (`sandboxtemplate-pool.yaml` in ops; chart pool
  render): add one `volumeClaimTemplate` — `name: workspace`, CephFS
  StorageClass, RWO, `5Gi` (tunable) — the template's emptyDir `workspace`
  volume is overridden by the VCT-backed volume of the same name
  (upstream `MergeVolumeClaimVolumes`).
- Chart: `warmPool.scratch.*` values (enabled/storageClass/size) rendered
  into the pool template only (`volumeClaimTemplatesPolicy: Allowed`
  already set); KIND e2e pool stays disabled.
- Broker: **no changes** (claims already carry no env/VCT; labels flow via
  `additionalPodMetadata`).
- Ops runbook: ResourceQuota PVC headroom (pool replicas × 1 PVC + claimed
  sandboxes), recycle the pool after the template change (members become
  stale by blueprint hash and are recreated per `updateStrategy`).
- S3 tiering: not applicable to ephemeral (emptyDir today — nothing
  offloads); reap keeps deleting the claim, which now also GCs the PVC.

Non-goals: persistent profile (broker PVC + S3 stays), per-chat StorageClass
overrides (template-identical VCTs are the only warm-compatible shape),
S3-backed warm pools (no upstream hooks), router re-pin.

MCP/frontend: none.

## Impact

- `open-websandbox-platform/chart/templates/sandboxtemplate.yaml` (pool
  branch: VCT render + values), `values.yaml`, `values.schema.json`
- `rust/`: none expected (verify with existing suite)
- KIND e2e: pool disabled — unchanged
- Ops: pool template + quota check + pool recycle runbook
