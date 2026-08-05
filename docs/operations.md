# Operations runbook

Day-2 operations for **open-sandbox**: tuning the warm pool, the idle
park/reap policy, capacity guardrails, common troubleshooting, rolling the
runtime image, and upgrade/rollback. See [`architecture.md`](architecture.md)
for how the pieces fit and [`deploy.md`](deploy.md) for the install it assumes.

The single-glance health script
[`agent-sandbox-platform/scripts/sandbox-status.sh`](../agent-sandbox-platform/scripts/sandbox-status.sh)
covers most of these checks at once (claims, active users, warm pool, pods,
control plane, quota, PVCs, node pressure; `-w` tails broker reap/park/error
lines):

```bash
agent-sandbox-platform/scripts/sandbox-status.sh        # snapshot
agent-sandbox-platform/scripts/sandbox-status.sh -w     # + recent reaper/error events
```

## Warm pool tuning

The warm pool pre-warms `N` sandboxes from `code-standard-v1` so a fresh
ephemeral claim binds instantly instead of paying cold-start. Config lives in
[`sandboxwarmpool-code-standard.yaml`](../agent-sandbox-platform/deploy/base/sandboxwarmpool-code-standard.yaml):

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
[`resourcequota.yaml`](../agent-sandbox-platform/deploy/base/resourcequota.yaml):

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
[`sandboxtemplate-code-standard.yaml`](../agent-sandbox-platform/deploy/base/sandboxtemplate-code-standard.yaml)):
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
[`networkpolicy-runtime.yaml`](../agent-sandbox-platform/deploy/base/networkpolicy-runtime.yaml)).
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
docker save ghcr.io/<owner>/open-sandbox-runtime:<tag> \
  | ssh ubuntu@<node> 'sudo microk8s.ctr images import -'
microk8s.ctr images ls | grep open-sandbox-runtime          # verify
```

Repeat for every worker (sandboxes can land on any gVisor node). If you push to
a registry instead, flip `imagePullPolicy` to `IfNotPresent` and reference the
full `ghcr.io/<owner>/...` tag in the manifest. See
[Build & load the images](deploy.md#build--load-the-images).

## Roll the runtime image

The runtime image is referenced by the SandboxTemplate
(`code-standard-v1`), so a rollout is a template patch + recycle of live pods:

1. Build and load the new image to every gVisor worker
   ([deploy.md](deploy.md#build--load-the-images)).
2. Bump the image tag in
   [`sandboxtemplate-code-standard.yaml`](../agent-sandbox-platform/deploy/base/sandboxtemplate-code-standard.yaml)
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
  [`upstream/`](../agent-sandbox-platform/upstream/) with a fresh `SHA256SUMS`,
  apply CRDs **before** the controller Deployment (forward-compatible CRDs
  first), then `kubectl rollout restart deploy/agent-sandbox-controller -n
  agent-sandbox-system`. Roll back by re-applying the v0.5.3 manifest; never
  downgrade CRDs without checking the controller supports them.
- **gVisor** (`runsc`): node-level, see [`infra/gvisor/README.md`](../infra/gvisor/README.md).
  Stage with `install-gvisor-node.sh` (inert until containerd restart), activate
  with `activate-gvisor-node.sh` (online-safe — running pods survive the
  containerd restart). If a node hosts the **CloudNativePG primary**, fail it
  over (`kubectl cnpg promote ...`) before activating — see the gVisor README's
  CNPG caveat. Re-verify with `manifests/gvisor-verify.yaml` after any node or
  MicroK8s upgrade.
