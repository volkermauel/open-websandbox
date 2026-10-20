# Proposal: Synchronous S3 snapshot on termination (Sandbox finalizer)

## Why

`workspace-s3-store-of-record` made S3 the store of record, but the offload
trigger is **broker-initiated only**: the reaper archives `/workspace` when a
chat goes idle and releases the sandbox afterwards. Any termination the broker
did not initiate — node drains/evictions, liveness kills racing the reaper,
manual `kubectl delete sandbox`, controller-driven rollouts — destroys the
sandbox writable layer with whatever wasn't archived yet. Scratch storage is
emptyDir by design, so the window between "last offload" and "pod death" is a
real data-loss window.

Sandbox→broker signaling cannot close it: sandbox egress is default-deny
(RFC1918 blocked, #48 posture), so a preStop hook cannot dial the broker
directly without opening a netpol hole we explicitly rejected.

Kubernetes already provides the right primitive: **finalizers**. A finalizer
on the Sandbox CR holds the object (and its pod) in Terminating while the
broker snapshots through the existing runtime API — the same
`S3Offload` seam the reaper uses — and removes the finalizer once the S3 put
is durable. Deletion ordering becomes: **snapshot → then pod dies**, enforced
by the API server instead of by timing.

## What Changes

- **Broker registers a finalizer** (`owui.dev/s3-snapshot`) on every Sandbox
  it manages in `s3-record` mode (claim path and cold-create path).
- **Termination handling**: when a watched Sandbox shows
  `metadata.deletionTimestamp`, the broker synchronously offloads the chat
  workspace via the runtime API and removes the finalizer only after the S3
  put completes. Fresh-snapshot dedupe (chat already has a current-generation
  snapshot) short-circuits to immediate removal — the common idle-reap path
  stays sub-second.
- **Degraded mode is safe**: if the broker is down mid-delete, the sandbox
  stays Terminating with the pod serving until the broker returns and
  finishes the offload. Self-healing; no lost data, no orphan pod (the
  finalizer blocks object removal, not the broker).
- **Drain belt**: SandboxTemplate pod spec gains
  `terminationGracePeriodSeconds: 45` and a
  `preStop: ["sleep", "30"]` so a direct **pod** eviction (node drain, which
  bypasses the Sandbox CR) still gives the broker's watch the window to
  archive while the runtime keeps serving. This is a bounded best-effort
  backstop, not a guarantee.
- **Config**: `BROKER_S3_TERMINATION_FINALIZER` (default `true` in s3-record
  mode; ignored otherwise). Chart value `broker.s3.terminationFinalizer`
  wires it through; base manifests kept in sync.

## Non-Goals

- Memory-inclusive snapshots (gVisor C/R — still blocked upstream,
  see `docs/container-snapshots.md`).
- Sandboxes outside `s3-record` mode (per-user-pvc keeps today's behavior;
  the PVC is the store of record there).
- Netpol changes or new in-sandbox credentials.

## Impact

- **Broker**: finalizer registration in the claim/create path;
  deletionTimestamp branch in the existing watch/reconcile; dedupe check
  against the snapshot generation stamp; one config knob.
- **Chart + base manifests**: template lifecycle (preStop sleep, grace) and
  the env knob.
- **e2e**: (1) manual `kubectl delete sandbox` → object stays Terminating
  until `latest.zst` lands in S3, then completes; (2) drain-style direct pod
  eviction → archive still lands (belt path); (3) reaper path unaffected
  (dedupe short-circuit latency regression guard).
- **Docs**: operations.md ordering guarantees + the Terminating-state
  expectation for on-call.
