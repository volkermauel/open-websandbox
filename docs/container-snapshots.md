# Container snapshots (checkpoint/restore) — feasibility

> Verdict record for [issue #134](https://github.com/volkermauel/open-websandbox/issues/134).
> Investigation date: 2026-08-15. Sources: upstream docs + source, GKE docs,
> gVisor source on our pinned release, our chart/configs. Full evidence log:
> the [investigation comment on issue #134](https://github.com/volkermauel/open-websandbox/issues/134#issuecomment-5301287859).

## Verdict in one paragraph

The upstream agent-sandbox **Snapshot feature cannot be used on plain
Kubernetes** — it is a Python-only SDK extension that drives GKE-proprietary
`podsnapshot.gke.io` custom resources, serviced by a closed-source GKE node agent
and control-plane controller, with snapshots stored in Cloud Storage. Nothing of
it exists in the agent-sandbox controller (v0.5.3 through v0.5.5), so re-vendoring
buys nothing. The *capability* it provides — **memory-inclusive** sandbox state
capture — is technically feasible in our environment, because it is built on
gVisor `runsc` checkpoint/restore, which is open and works on plain k8s +
containerd under systrap. But the k8s-native orchestration layer GKE provides
(trigger via CRI, restore into a new pod) **does not exist open-source today**:
the gVisor containerd-shim C/R support ([google/gvisor#13326](https://github.com/google/gvisor/pull/13326))
and containerd's CRI `CheckpointContainer` for gVisor
([containerd/containerd#12280](https://github.com/containerd/containerd/issues/12280))
are both still open. So: **not possible today without building (and maintaining)
node-level orchestration ourselves; possible at medium effort once the upstream
shim support lands and we bump the runsc pin.**

## What the upstream feature is

| Piece | Where it lives | On plain k8s? |
|---|---|---|
| `PodSnapshotSandboxClient`, `SnapshotEngine` | Python SDK only (`k8s_agent_sandbox/gke_extensions/snapshots/`), already present in v0.5.3 | Client yes, but it only writes CRs |
| `PodSnapshotManualTrigger`, `PodSnapshot`, `PodSnapshotPolicy`, `PodSnapshotStorageConfig` CRDs | `podsnapshot.gke.io` group — **GKE-provided** | **No** — no CRDs, no reconciler |
| Node agent (runs `runsc` checkpoint, streams to storage) | GKE node image, closed | **No** — we'd build a DaemonSet |
| Snapshot controller + matching (spec hash, machine series, gVisor version) | GKE control plane, closed | **No** |
| Snapshot storage | Cloud Storage buckets only | **No** — we'd use S3 |
| Suspend/resume lifecycle | agent-sandbox Sandbox CR `spec.operatingMode` | **Yes** — we already use it (park/resume) |

What a snapshot contains (GKE docs, i.e. runsc C/R semantics): full application
state (memory, threads, FDs, registers), container rootfs, emptyDir, tmpfs.
**Persistent volumes are never checkpointed.** On restore the pod gets a new IP
and hostname, wall-clock jumps to now, external connections are dropped;
listening sockets, loopback and Unix-domain sockets survive.

## Fit for our environment

| Requirement | Our environment | Status |
|---|---|---|
| Core C/R incl. memory | runsc pin `release-20260727.0`; C/R is sentry-internal, **systrap-compatible**, no `/dev/kvm` | ✅ |
| In-sandbox capture trigger | App-driven C/R annotations (`dev.gvisor.internal.checkpoint.path`/`.enable`, `/proc/gvisor/checkpoint`) **landed Jun 2026, verified in our pin** | ✅ mechanism |
| Filesystem state | rootfs + emptyDir + tmpfs captured; persistent profile's PVC unaffected (persists anyway) | ✅ |
| Checkpoint storage | S3 machinery from #52 (`rust/runtime/src/snapshot.rs`, broker S3 client) reusable for state files | ✅ reuse |
| **Restore into a new pod** | gVisor shim restore-from-annotation (`dev.gvisor.internal.restore.host-image-path`) is **unmerged** (PR #13326); containerd CRI path open (#12280); kubelet `/checkpoint` is forensic-only, no k8s restore API exists | ❌ **blocker** |
| Network identity across restore | New pod IP; broker re-resolves per request (`rust/broker/src/resolve.rs`) | ✅ (WS terminals reconnect) |
| Node/CPU/version compatibility | Homogeneous dedicated gvisor pool + uniform runsc release pin; `dev.gvisor.internal.cpufeatures` available if we ever mix CPUs | ✅ |
| Controller version skew | v0.5.3 has no snapshot code; nothing to re-vendor | ➖ N/A |
| Pod shape | Sandbox pods are single-container → whole-sandbox C/R granularity fits | ✅ |

## Risks

- **Checkpoint size**: the image contains all memory pages (GBs per sandbox);
  S3 cost/bandwidth and capture pause time scale with it. Mitigations:
  `--exclude-committed-zero-pages`, compression trade-offs (background restore
  requires `--compression=none`).
- **Connection break on resume**: external conns (broker-proxied WS terminals)
  are terminated at restore; clients must reconnect. #135's scrollback flush
  softens this.
- **Version skew**: checkpoints must be restored by the same runsc release and on
  CPU-compatible nodes. Our uniform pin satisfies this today; every runsc bump
  invalidates existing checkpoints.
- **Upstream dependency timing**: PR #13326 (+829/−3, 10 files — small but
  unreviewed/merge date unknown) and containerd#12280 have no ETA.
- **Security**: a checkpoint image is a full memory dump (may contain secrets);
  it must be encrypted at rest and access-controlled like the workspace tar
  already is. A node-level capture agent (alternative trigger path) needs
  containerd-socket privileges — a new attack surface in a currently
  default-deny setup.

## Relationship to what we have

- **S3 workspace snapshots (#52)**: files-only, works under runc too → remains
  the portable baseline. Container snapshots add memory + rootfs + emptyDir,
  gVisor-only. **Complement.**
- **Park/resume**: same lifecycle entry point (`operatingMode: Suspended`);
  C/R adds the memory dimension that park lacks.
- **#130 (long sessions / 120s suspend)**: the primary beneficiary — a resumed
  session would get its processes, package layer and `/tmp` back, not just PVC
  files.
- **Warm pool**: orthogonal (pre-provisioning); unaffected.

## Recommended path (ordered)

1. **Now (no new deps)**: optional PoC on the KIND-gVisor cluster — annotate the
   `SandboxTemplate` pod with `dev.gvisor.internal.checkpoint.path` (pointing at
   a volume) + `.enable=true`, trigger via `/proc/gvisor/checkpoint` from the
   runtime, confirm state files land on the volume, offload to S3. Capture-only;
   restore stays manual (`runsc restore` CLI). Effort **S–M**.
2. **Track upstream**: watch google/gvisor#13326 and containerd/containerd#12280.
   When shim C/R ships in a runsc release, bump `GVISOR_RELEASE`
   (`infra/gvisor`) — restore becomes a pod annotation + host-path staging.
3. **Build the restore agent**: privileged DaemonSet (or the app-driven path +
   volume) that materializes S3 checkpoints onto node-local disk before resume
   and sets node affinity. Effort **M**.
4. **Wire the lifecycle**: park → capture + offload; resume → pre-stage + restore
   annotation; checkpoint metadata (runsc release, spec hash) + reaper rules +
   #130 e2e extension. Effort **M**.

Total today (bypassing upstream): **L**. After (1)–(2) land: **M**.

## Open questions

See the issue thread for the full list (privileged DaemonSet acceptability,
memory-image retention budget, PTY reconnect UX, patched-runsc vs wait-for-upstream).
