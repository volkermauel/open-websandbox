# Operations runbook

Day-2 operations for **open-websandbox**: tuning the warm pool, the idle
park/reap policy, capacity guardrails, common troubleshooting, rolling the
runtime image, and upgrade/rollback. See [`architecture.md`](architecture.md)
for how the pieces fit and [`deploy.md`](deploy.md) for the install it assumes.

The single-glance health script
[`open-websandbox-platform/scripts/sandbox-status.sh`](../open-websandbox-platform/scripts/sandbox-status.sh)
covers most of these checks at once (claims, active users, warm pool, pods,
control plane, quota, PVCs, node pressure; `-w` tails broker reap/park/error
lines):

```bash
open-websandbox-platform/scripts/sandbox-status.sh        # snapshot
open-websandbox-platform/scripts/sandbox-status.sh -w     # + recent reaper/error events
```

## Observability: Prometheus metrics + OpenTelemetry tracing

The Rust broker/runtime ship two independent observability surfaces (issue

# 83 / decision D9)

- **Prometheus `/metrics` (always-on).** The `metrics` facade fronts a single
  per-process `metrics-exporter-prometheus` recorder. `/metrics` is served
  unconditionally — it is **not** affected by the OTLP feature flag below.
- **OpenTelemetry tracing (always-on SDK, opt-in exporter).** A `tracing`
  subscriber with a `fmt` layer is always installed. An OTLP span exporter is
  wired in **only** when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. With the
  endpoint unset, tracing degrades to a no-op (fmt-only) — boot/serve never
  depend on a collector being reachable.

### OTLP transport: `OTEL_EXPORTER_OTLP_PROTOCOL`

When the OTLP exporter is opted into (`OTEL_EXPORTER_OTLP_ENDPOINT` set), the
transport is selected by `OTEL_EXPORTER_OTLP_PROTOCOL`:

| Value              | Transport                                   |
| ------------------ | ------------------------------------------- |
| `grpc` (default)   | OTLP/gRPC over tonic (`:4317`)              |
| `http`             | OTLP/HTTP-protobuf over reqwest (`:4318`)   |
| `http/protobuf`    | alias of `http`                             |

Any other value makes the broker fall back to fmt-only tracing (the build
error is logged; it is never fatal). `grpc` stays the default to preserve the
D9 behaviour (issue #83 Q2).

### Feature flag: `telemetry-otlp` (default-on) & slim builds

The OTLP exporter crate (`opentelemetry-otlp`, which pulls tonic/prost/h2 for
gRPC and reqwest for HTTP) is behind the **default-on** `telemetry-otlp`
feature. Slim/no-OTel control-plane images compile it out to drop the whole
gRPC/HTTP exporter stack (the `metrics` facade + `/metrics` and the
`opentelemetry` tracing SDK remain always-on):

```bash
# From rust/ — build the broker WITHOUT the OTLP stack (tonic/prost/h2 absent):
cargo build --release -p broker --no-default-features

# Prove the gRPC stack is gone (prints nothing):
cargo tree -p broker --no-default-features | grep -E 'tonic|prost|opentelemetry-otlp'
```

### Broker binary size (stripped, `--release`)

Measured for the Rust broker on amd64 (issue #83). The production distroless image
currently ships the binary **unstripped (~37–40 MiB)** — accepted as a single
executable (debug symbols retained for prod backtraces; the D13 ~15–30 MiB target is
revised to ~40 MiB accordingly). The stripped reference below is the floor reachable by
adding `strip = true` to `[profile.release]` (or a Dockerfile strip step). The OTLP
opt-out (`--no-default-features`) trims ~1.9 MiB stripped / ~2.8 MiB unstripped.

| Build                              | Stripped size |
| ---------------------------------- | ------------: |
| default (`telemetry-otlp` ON)      |  27,667,656 B (~26.4 MiB) |
| `--no-default-features` (OFF)      |  25,695,024 B (~24.5 MiB) |
| **Delta (OTLP gRPC+HTTP stack)**   |  **1,972,632 B (~1.9 MiB)** |

`cargo fmt` / `cargo clippy -- -D warnings` / `cargo test --all` are green in
**both** feature configurations (default and `--no-default-features`).

## Warm pool tuning

The warm pool pre-warms `N` sandboxes from `code-standard-v1` so a fresh
ephemeral claim binds instantly instead of paying cold-start. Config lives in
[`sandboxwarmpool-code-standard.yaml`](../open-websandbox-platform/deploy/base/sandboxwarmpool-code-standard.yaml):

```yaml
spec:
  replicas: 2                      # free, always-Running sandboxes
  sandboxTemplateRef:
    name: code-standard-v1
```

Tune `replicas` to roughly **peak concurrent new sessions per minute**. Too few
and first-call latency jumps (the broker waits up to `BROKER_CLAIM_TIMEOUT_SECONDS`
for `Ready`, then returns 504); too many and you burn CPU/RAM/pods while idle.

```bash
kubectl -n agent-sandbox-runtime scale sandboxwarmpool/code-standard-warmpool --replicas=4
kubectl -n agent-sandbox-runtime get sandboxwarmpools
```

Each warm pod costs the SandboxTemplate's requests (250 m CPU / 512 Mi) and
counts against the runtime-namespace `ResourceQuota` (see
[Capacity limits](#capacity-limits)) — the quota's `count/pods: "20"` is the
hard ceiling on *warm + claimed* pods combined. Note that **persistent** sandboxes
always cold-start (warm pods have no PVC), so the warm pool only serves the
ephemeral profile.

## Idle policy: park & reap

> Parking preserves **files only** (PVC). Whether process/memory state can also be
preserved via gVisor checkpoint/restore is assessed in
[Container snapshots (C/R) feasibility](container-snapshots.md) (#134).

The broker's reaper runs every 60 s and acts on the `broker-last-used`
annotation (see [Lifecycle](architecture.md#lifecycle)). Defaults:

| Knob (broker env) | Default | Effect |
|-------------------|---------|--------|
| `BROKER_IDLE_TTL_SECONDS` | `120` | **Ephemeral** claim reaped → sandbox released back to the warm pool. |
| `BROKER_PARK_IDLE_SECONDS` | `120` | **Persistent** sandbox parked (`operatingMode: Suspended`): pod deleted, node freed, PVC retained. |
| `BROKER_REAP_SECONDS` | `604800` (7 d) | **Persistent** claim + PVC fully deleted. |

Notes when changing these:

- **Park vs. reap trade-off.** Parking frees the node but keeps the PVC (and its
  storage quota). Lower `BROKER_PARK_IDLE_SECONDS` to reclaim nodes faster at
  the cost of a 1–6 s cold resume on the next request; raise it to keep chats
  instant at the cost of node occupancy. Lower `BROKER_REAP_SECONDS` to reclaim
  storage faster (and lose parked-but-unused workspaces sooner).
- **Ephemeral has no park.** Ephemeral sandboxes are always `Running` while
  claimed; idle just reaps them. Tune `BROKER_IDLE_TTL_SECONDS`, not park.
- The reaper is **stateless and idempotent** — restarting the broker re-derives
  ownership from labelled claims/sandboxes, so it never orphans sessions. A
  parked sandbox that the broker loses track of will simply be reaped at
  `BROKER_REAP_SECONDS`.

```bash
# Watch park/reap decisions live
kubectl -n agent-sandbox-system logs deploy/owui-broker -f \
  | grep -iE 'park|reap|suspend|operatingMode'
```

## Capacity limits

Hard guardrails for the `agent-sandbox-runtime` namespace, from
[`resourcequota.yaml`](../open-websandbox-platform/deploy/base/resourcequota.yaml):

| Resource | Request cap | Limit cap | Other |
|----------|-------------|-----------|-------|
| `pods` (`count/pods`) | — | — | **20** total (warm + claimed) |
| `cpu` | 10 | 30 | |
| `memory` | 20 Gi | 40 Gi | |
| `ephemeral-storage` | 20 Gi | 120 Gi | |
| `persistentvolumeclaims` | — | — | **50** |
| `requests.storage` | — | — | 200 Gi |

The `LimitRange` in the same file gives any pod without explicit resources a
sane default: request 250 m / 512 Mi / 1 Gi, limit 2 CPU / 4 Gi / 4 Gi.

Per-sandbox caps (from
[`sandboxtemplate-code-standard.yaml`](../open-websandbox-platform/deploy/base/sandboxtemplate-code-standard.yaml)):
request 250 m / 512 Mi / 2 Gi, limit **2 CPU / 4 Gi / 12 Gi ephemeral**. Plus
runtime-enforced soft limits: command timeout (`DEFAULT_TIMEOUT` 120 s, max
`MAX_TIMEOUT` 600 s), stdout/stderr capped at `MAX_OUTPUT_BYTES` (1 MiB),
`RLIMIT_NPROC` (`MAX_PROCS` 256), and `/tmp` on a **tmpfs with a hard ENOSPC cap**
(2 Gi) so a sandbox can't fill node disk.

If you hit a quota wall the symptom is pods stuck `Pending` / claims not reaching
`Ready`:

```bash
kubectl -n agent-sandbox-runtime describe resourcequota
kubectl -n agent-sandbox-runtime get events --sort-by=.lastTimestamp | tail
```

Raise the `ResourceQuota.hard` (and the underlying node capacity) — not the
per-pod template limits — to admit more concurrent sandboxes.

## Backup & Restore (per-user PVCs)

The only durable state in open-websandbox is each user's `/workspace`, held on a
per-user RWX PVC named `workspace-p-<12-hex>` in `agent-sandbox-runtime` (#140:
`broker.persistentMode: per-user-pvc`; the hash is `sha256(user_id)[:12]`, so
the name is **deterministic per user** — a restored PVC re-binds to the same
user when the broker recreates it on next access). Each chat lives in its own
`chats/<sha256(user/session)[:12]>` subPath of that PVC, so snapshotting the
user PVC captures every chat of that user. In `shared-subpath` mode the same
applies with ONE shared `workspace-shared` PVC (snapshots cover all users).
In `empty-dir` mode the durable state is the S3 bucket instead (#142), and in
the PVC modes with `broker.s3.enabled` hybrid tiering applies: a reaped chat's
data lives in S3 until its next resolve, so restore BOTH the PVCs and the S3
bucket. Ephemeral sandboxes use an `emptyDir` and are not backed up.

> **The PVCs are unencrypted at rest.** Backups do not give you
> confidentiality. Enable encryption at the **storage layer** (CephFS
> encryption-at-rest, CSI/LUKS encryption, or Velero with a KMS-backed
> Restic/Kopia integration) before relying on any of the mechanisms below.

Two mechanisms, choose per recovery objective:

### 1. CSI VolumeSnapshots (preferred — fast, storage-native, point-in-time)

1. Ensure a `VolumeSnapshotClass` exists for your RWX StorageClass and that it
   is the default for that provisioner.
2. Schedule periodic `VolumeSnapshot` resources per PVC. The platform ships no
   scheduler, so run one via a CronJob that lists `workspace-p-*` PVCs in
   `agent-sandbox-runtime` and creates/rotates snapshots (e.g. keep 7 daily).
3. **Restore** by creating a new PVC that uses the snapshot as its data source:

   ```yaml
   apiVersion: v1
   kind: PersistentVolumeClaim
   metadata:
     name: workspace-p-<hash>          # must match the user's deterministic name
     namespace: agent-sandbox-runtime
   spec:
     accessModes: ["ReadWriteMany"]
     storageClassName: <your-rwx-class>
     resources: { requests: { storage: 10Gi } }
     dataSource:
       name: <snapshot-name>
       kind: VolumeSnapshot
       apiGroup: snapshot.storage.k8s.io
   ```

   The broker recreates the `SandboxClaim` on the user's next request and binds
   the restored PVC by name.

Trade-off: snapshots are tied to one storage backend; fast to take/restore but
not portable across storage classes.

### 2. Velero, namespace-scoped (portable, file-level)

Install/run Velero scoped to the runtime namespace and enable file-system
backups (Restic/Kopia) for the `workspace-p-*` PVCs:

```bash
velero backup create sandbox-pvcs-$(date +%F) \
  --include-namespaces agent-sandbox-runtime \
  --include-resources persistentvolumeclaims,persistentvolumes \
  --snapshot-volumes=false                # use file-level (Restic/Kopia), not CSI snapshots

velero restore create --from-backup sandbox-pvcs-<date> \
  --namespace-mappings agent-sandbox-runtime:agent-sandbox-runtime
```

Trade-off: portable across storage classes and Velero can encrypt backups at
rest (KMS/gpg), but file-level backup/restore is slower than CSI snapshots and
consumes object-storage space per PVC.

### Notes for both

- **RWX coordination:** a `Suspended` (parked) pod's PVC stays *Bound* and is
  still snapshottable — there is no need to wake a sandbox to back it up.
- **Restore-after-reap:** once a user's claim+PVC are reaped
  (`BROKER_REAP_SECONDS`, 7 d), they are gone. Restoring the PVC (by name)
  alone will **not** recreate the `SandboxClaim` — the broker does that on the
  user's next request and binds the restored PVC. Verify the restored PVC name
  matches `workspace-p-<hash>` or it will not bind.
- **Test restores** regularly; an untested backup is not a backup.

## Troubleshooting

### Sandbox stuck `NotReady` (claim never binds)

```bash
kubectl -n agent-sandbox-runtime get sandboxclaims,sandboxes,pods -o wide
kubectl -n agent-sandbox-runtime describe sandbox <name>
kubectl -n agent-sandbox-runtime describe pod <pod-name>     # events at the bottom
```

Common causes:

- **No warm sandbox free** and cold-start in progress — wait up to
  `BROKER_CLAIM_TIMEOUT_SECONDS` (60 s); if it keeps timing out, scale the warm
  pool up or check node pressure.
- **gVisor RuntimeClass missing** on the scheduling node — the pod event will
  say `runtimeclass "gvisor" not found` or handler errors. Re-run
  [`infra/gvisor/activate-gvisor-node.sh`](../infra/gvisor/activate-gvisor-node.sh)
  on that node and verify with
  [`manifests/gvisor-verify.yaml`](../infra/gvisor/manifests/gvisor-verify.yaml).
- **ResourceQuota exhausted** (pods/cpu/storage) — see
  [Capacity limits](#capacity-limits). Persistent sandboxes also need a free
  PVC slot and `requests.storage` headroom.
- **PVC not binding** (persistent) — check the `cephfs` (or your RWX)
  StorageClass and storage backend health: `kubectl -n agent-sandbox-runtime
  get pvc`.

### Terminal returns `429 max N terminals reached`

Each runtime pod caps concurrent interactive PTYs at `MAX_TERMINAL_SESSIONS`
(default 8). A 429 means leaked/abandoned terminals on that pod. The broker
closes the upstream WS on client disconnect so the runtime reaps the PTY in its
`finally` block — a persistent leak usually points to a WS that never closed
cleanly. Mitigations:

- Raise `MAX_TERMINAL_SESSIONS` in the SandboxTemplate (costs fds/RAM per pod).
- Confirm the client is actually closing the WS (the broker logs
  `terminal ws ... failed` on abnormal upstream ends).
- Recycle the affected pod (it clears the in-memory terminal table): see
  [Roll the runtime image](#roll-the-runtime-image).

### DNS / package-install egress failures

Egress is locked to public DNS (8.8.8.8/1.1.1.1) and HTTPS+HTTP (443/80) to the
**public internet only**, with all RFC1918/link-local blocked (see
[`networkpolicy-runtime.yaml`](../open-websandbox-platform/deploy/base/networkpolicy-runtime.yaml)).
If `pip`/`npm`/`git` hang or fail inside a sandbox:

- DNS failures → check the resolver IPs are reachable from the node
  (`dnsPolicy: None` pins 8.8.8.8/1.1.1.1; if your network blocks them, edit
  the SandboxTemplate `dnsConfig.nameservers` **and** the matching NetworkPolicy
  egress `ipBlock` for port 53).
- HTTPS failures → confirm the destination isn't on a private CIDR (it will be
  silently dropped) and that the node's egress to 443/80 works. A connection to
  an internal service is *expected* to fail.
- IPv6-only registries need the IPv6 egress rule present (it is, in the shipped
  manifest: `::/0` except ULA/link-local).

### Image not found / `imagePullPolicy: Never`

The shipped manifests set `imagePullPolicy: Never` and expect images
pre-loaded into each worker's containerd. An `ErrImageNeverPull` /
`ImagePullBackOff` means that node doesn't have the image:

```bash
# on the node where the pod is scheduled (MicroK8s):
docker save ghcr.io/<owner>/open-websandbox-runtime:<tag> \
  | ssh ubuntu@<node> 'sudo microk8s.ctr images import -'
microk8s.ctr images ls | grep open-websandbox-runtime          # verify
```

Repeat for every worker (sandboxes can land on any gVisor node). If you push to
a registry instead, flip `imagePullPolicy` to `IfNotPresent` and reference the
full `ghcr.io/<owner>/...` tag in the manifest. See
[Build & load the images](deploy.md#2-build-load-the-images).

### Reaper stuck / error-loop

Symptom: idle sandboxes never park/reap — PVCs and pods accumulate, parked
pods stay `Running`, `BROKER_REAP_SECONDS` never fires. The broker's reaper
(`_reaper_loop`) runs as an asyncio task and **catches every exception**, so a
failing iteration keeps the loop alive but logs `reaper iteration error` and
makes no progress — a repeating error every ~30 s is the signature.

```bash
kubectl -n agent-sandbox-system logs deploy/owui-broker --tail=200 \
  | grep -iE 'reaper iteration error|park|reap|suspend'
kubectl -n agent-sandbox-runtime get sandboxclaims,sandboxes -o wide   # stale 'Running' that should be 'Suspended'
kubectl -n agent-sandbox-system rollout status deploy/agent-sandbox-controller
```

Common causes:

- **Controller down/unhealthy** → reaper can't list/read `Sandbox`/`SandboxClaim`
  status, so park/reap decisions never fire. Restart the controller first.
- **API-server throttle / RBAC** → the broker ServiceAccount lost read on the
  CRDs; each iteration throws and is swallowed.
- **A poison claim** (bad annotation/status) makes every iteration throw at the
  same object.

The reaper is **stateless and idempotent** — restarting the broker is always
safe; it re-derives ownership from labelled claims/sandboxes and never orphans
sessions:

```bash
kubectl -n agent-sandbox-system rollout restart deploy/owui-broker
```

### WS-proxy 504 (proxyTimeout 660 vs. MAX_TIMEOUT 600)

The broker holds its upstream `httpx` client open for
`BROKER_PROXY_TIMEOUT_SECONDS` = **660 s**, deliberately ~60 s **above** the
runtime's `MAX_TIMEOUT` = **600 s**, so a legitimately long command (up to the
600 s cap) always completes *inside* the broker window. Under normal operation
a long `POST /execute` returns at ≤ 600 s and never hits 660 s. A 504 means one
of:

1. **A sandboxed command overshot `MAX_TIMEOUT`** (hung loop, deadlock, or a
   command that ignores SIGTERM). The broker waits the full 660 s then returns
   504. Confirm in the broker log (a 660 s gap on one request) and tighten the
   command / check `MAX_OUTPUT_BYTES` truncation in the runtime response.
2. **An intermediary in front of the broker has a shorter idle/timeout and
   returns 504 first.** This is the common case: a reverse proxy, ingress, or
   Gateway with a 60 s / 120 s / 300 s idle timeout will 504 a long call long
   before the broker's 660 s. Raise that intermediary's timeout to ≥ 660 s
   (the broker's), or lower your expected command runtime below it.
3. **The sandbox-router's own HTTP proxy timeout** (`--proxy-timeout=180s`) can
   return early on a long *non-streaming* HTTP call on the broker→router→runtime
   path. (The interactive terminal is a WebSocket upgrade and is not bound by
   this HTTP timeout.)

```bash
# Is it the intermediary or the broker? Time one long execute end-to-end:
time curl -sS http://owui-broker.agent-sandbox-system.svc:8080/execute \
  -H 'Authorization: Bearer <token>' -H 'X-User-Id: u' -H 'X-Session-Id: s' \
  -d '{"command":"sleep 590"}'      # ~590 s should succeed; ~60/180 s 504 = intermediary
```

### PVC Pending (persistent workspace)

Symptom: a persistent user's `SandboxClaim` is bound but their
`workspace-p-<hash>` PVC is stuck `Pending`; the sandbox pod never starts.

```bash
kubectl -n agent-sandbox-runtime get pvc -o wide
kubectl -n agent-sandbox-runtime describe pvc workspace-p-<hash>   # events at the bottom
kubectl -n agent-sandbox-runtime describe resourcequota           # quota headroom?
```

Common causes:

- **StorageClass missing/misnamed** (`sharedPvc.storageClass` / your RWX
  class) — event says `storageclass.storage.k8s.io "..." not found`.
- **RWX provisioner / storage backend unhealthy** (Ceph down, no MDS) — event
  says `failed to provision volume ... Waiting for a volume to be created`.
- **Quota exhausted** — `persistentvolumeclaims: "50"` or `requests.storage:
  "200Gi"` full (see [Capacity limits](#capacity-limits)).
- **`WaitForFirstConsumer` binding + no schedulable gVisor node** — the PVC
  waits for a pod that the taint/node-selector keeps off every node.

### sandbox-not-ready (SandboxClaim stays NotReady)

The broker returns **504** `sandbox claim ... not ready in 60s` when a claim
does not reach `Ready` within `BROKER_CLAIM_TIMEOUT_SECONDS` (60 s). This is a
**different 504 from the WS-proxy one** — note the message names the *claim*,
not a timeout. (See also the existing [Sandbox stuck `NotReady`](#sandbox-stuck-notready-claim-never-binds)
entry.) Causes, in order of frequency:

- **Warm pool empty** + cold-start > 60 s → scale the warm pool up / check node
  pressure (see [Warm pool tuning](#warm-pool-tuning)).
- **gVisor RuntimeClass missing** on the target node (pod event:
  `runtimeclass "gvisor" not found`) → re-run the gVisor activate playbook.
- **ResourceQuota exhausted** (pods/cpu/storage) → [Capacity limits](#capacity-limits).
- **PVC Pending** (persistent) → entry directly above.

### Silent partial-outage — what `/healthz` does NOT check

`GET /healthz` returns `{"status":"ok"}` **unconditionally** — it proves only
that the broker process answers HTTP. It does **not** verify:

- warm-pool `readyReplicas` (the pool can be drained — every claim cold-starts),
- PVC binding (all persistent PVCs can be `Pending`),
- gVisor nodes schedulable (all tainted away / drained / cordoned),
- the sandbox-router reachable (broker can't proxy to any runtime),
- the agent-sandbox controller healthy (no reconciliation → no new claims),
- ResourceQuota headroom (quota full → new claims stuck),
- the reaper progressing (see entry above),
- the storage backend up.

So a green `/healthz` can mask a fully broken platform. Do not use `/healthz`
as your only uptime signal. Monitor instead:

```bash
# Single-glance reality check (claims, warm pool, pods, quota, PVCs, nodes):
open-websandbox-platform/scripts/sandbox-status.sh
# Or the specific signals that matter:
kubectl -n agent-sandbox-runtime get sandboxwarmpool -o jsonpath='{.items[*].status.readyReplicas}'
kubectl -n agent-sandbox-runtime get pvc --no-headers | awk '$3!="Bound"' | wc -l   # Pending PVC count
kubectl -n agent-sandbox-system  rollout status deploy/agent-sandbox-controller deploy/sandbox-router
```

## Roll the runtime image

The runtime image is referenced by the SandboxTemplate
(`code-standard-v1`), so a rollout is a template patch + recycle of live pods:

1. Build and load the new image to every gVisor worker
   ([deploy.md](deploy.md#2-build-load-the-images)).
2. Bump the image tag in
   [`sandboxtemplate-code-standard.yaml`](../open-websandbox-platform/deploy/base/sandboxtemplate-code-standard.yaml)
   and `kubectl apply` it. **Existing pods keep the old image** — the template
   change only affects *newly created* pods.
3. Recycle live pods so they pick up the new image:
   - **Warm pool:** `kubectl -n agent-sandbox-runtime delete pods -l
     app=code-standard,profile=ephemeral` (or scale the warm pool to 0 and back)
     — the warm pool rebuilds from the new template.
   - **Persistent sandboxes:** resume each by briefly setting
     `operatingMode: Running` won't help (the pod may already be up). To force a
     rebuild without losing the PVC, patch the sandbox `operatingMode: Suspended`
     then back to `Running`, or simply delete the pod and let the controller
     recreate it. The PVC is retained (`shutdownPolicy: Retain`), so files
     survive.
4. Verify a claimed sandbox runs the new image (`kubectl -n
   agent-sandbox-runtime exec <pod> -- ...`) and watch broker logs for
   `Ready`/errors.

The broker and router are ordinary Deployments — roll them with a normal
`kubectl rollout restart deploy/owui-broker -n agent-sandbox-system` (the broker
is stateless and recovers claims on restart) / `deploy/sandbox-router`.

## Upgrade & rollback

- **Platform images** (broker/router/runtime): bump tag → apply → roll, as
  above. Roll back by re-applying the previous tag and recycling. The broker
  keeps no session DB (stateless recovery from labelled claims), so a broker
  rollback doesn't lose user sandboxes.
- **Upstream controller** (`agent-sandbox` v0.5.3): a new minor is a CRD
  upgrade — read its release notes, re-vendor the new manifest into
  [`upstream/`](../open-websandbox-platform/upstream/) with a fresh `SHA256SUMS`,
  apply CRDs **before** the controller Deployment (forward-compatible CRDs
  first), then `kubectl rollout restart deploy/agent-sandbox-controller -n
  agent-sandbox-system`. Roll back by re-applying the v0.5.3 manifest; never
  downgrade CRDs without checking the controller supports them.
- **gVisor** (`runsc`): node-level, see [`infra/gvisor/README.md`](../infra/gvisor/README.md).
  Stage with `install-gvisor-node.sh` (inert until containerd restart), activate
  with `activate-gvisor-node.sh` (online-safe — running pods survive the
  containerd restart). If a node hosts a **stateful primary** (e.g. a database),
  fail it over before activating (per its operator's docs) — see the gVisor
  README's stateful-primary caveat. Re-verify with `manifests/gvisor-verify.yaml`
  after any node or MicroK8s upgrade.

### Helm upgrade & version skew

When the platform is Helm-managed (the usual install), the image rollout above is a
tag bump through the release, and rollback is revision-based:

```bash
# Upgrade every platform image to a new tag (broker, router AND runtime):
helm upgrade open-websandbox open-websandbox-platform/chart \
  --reuse-values --set imageTag=<new-tag> --wait --timeout 5m

# Roll back to the previous revision (chart + values as they were):
helm history open-websandbox          # pick the revision to return to
helm rollback open-websandbox <rev> --wait --timeout 5m
```

**Per-user PVCs survive both directions** — `shutdownPolicy: Retain` keeps the volume
when the runtime pod is recreated, and the controller reattaches it on the next claim
(park/resume). This path is exercised end-to-end by
[`tests/e2e/test_upgrade_rollback.py`](../tests/e2e/test_upgrade_rollback.py) — an
opt-in lane (`E2E_UPGRADE=1`) that writes a marker file to a persistent sandbox,
upgrades, asserts the file survives, rolls back, and asserts the image reverts.
The lane runs **weekly in CI** (`.github/workflows/e2e-upgrade.yml`, Mondays 04:05
UTC + `workflow_dispatch`) — not per-PR, since the mechanics only change with the
chart's image/PVC plumbing.

**Version-skew rules**

- The three platform images roll together under the single chart `imageTag`. The broker,
  router and runtime speak a small, versioned HTTP/WS contract; skew *within* a chart
  release is not a supported combination — upgrade all three atomically via `helm upgrade`.
- The broker is stateless (it recovers claims from labelled `SandboxClaim` objects), so a
  broker/router rollback never strands user sandboxes; only the runtime image affects
  what is *inside* a sandbox pod.
- Live persistent sandbox pods keep their old runtime image until recycled — the
  `SandboxTemplate` change only affects newly built pods. Recycle them (see
  [Roll the runtime image](#roll-the-runtime-image)) to force the new image.
- The vendored upstream controller is **upstream-driven**: CRD conversion-webhook needs
  flow from `kubernetes-sigs/agent-sandbox`, not from this chart. Follow the CRD-ordering
  rule above (forward-compatible CRDs first) whenever the vendored version moves.

## Node drain & terminal resume (issue #129)

When a runtime node is drained — or a sandbox pod is evicted for any reason — the
**process state inside the pod dies** (the shell, its environment, running jobs). What
survives, and what a reconnecting client gets:

| What | Eviction outcome |
| --- | --- |
| PVC files (persistent profile) | ✅ survive — the controller recreates the pod and reattaches the volume |
| Scrollback tail (last `TERMINAL_SCROLLBACK_BYTES`) | ✅ survives — the runtime traps SIGTERM and flushes it to `<workspace>/.open-websandbox/scrollback/<id>.log` before exiting; the recreated session preloads it |
| Terminal session id | ✅ survives — the broker's `ensure_pty` reuses the same id on the new pod |
| Shell process, `cwd`, environment, running jobs | ❌ lost — a resumed terminal starts a **fresh shell** with the replayed tail |
| Ephemeral (emptyDir) workspaces | ❌ lost with the pod |

For a **transient WS drop without pod death** (broker restart, network blip, client
refresh) nothing is lost at all: the runtime *detaches* instead of killing the PTY —
output keeps draining into the scrollback ring — and the next attach to the same id
resumes the **same shell** with the tail replayed first.

**Contract notes**

- One live WS relay per terminal: a second concurrent attach is closed with `4009`;
  an unknown/ended session still closes with `4004`.
- `POST /api/terminals` with an existing live id **reuses** that session (this is how the
  broker's reconnect path resumes a shell); only a dead session is recreated.
- A detached terminal survives for `TERMINAL_DETACH_TTL_SECS` (default 900) before the
  runtime's idle sweep reaps it — detached PTYs never leak to `MAX_TERMINAL_SESSIONS`.
- `DELETE /api/terminals/{id}` also removes the flushed scrollback file: a killed
  terminal never replays.
- Both knobs are chart values: `sandboxTemplate.terminalScrollbackBytes` (default
  128 KiB; `0` disables capture, replay and flush) and `sandboxTemplate.terminalDetachTtlSecs`.

Cross-node drain needs an **RWX** storage class for the persistent profile (see
[PVC Pending](#pvc-pending-persistent-workspace)); single-node KIND's default RWO class
works because the recreated pod lands on the same node. The whole flow is exercised
end-to-end by [`tests/e2e/test_node_drain.py`](../tests/e2e/test_node_drain.py) — a
lane (`E2E_DRAIN=1`, run in CI as the `drain` arm of the `e2e-pvc` matrix on the
per-user PVC profile) that opens a terminal, deletes the sandbox pod, and asserts
the reconnect replays the pre-eviction tail with the marker file intact. A *real*
`kubectl drain` (cordon + evict from a node that stays down) additionally needs a
multi-node cluster — future work, tracked separately from this lane.
