# Deployment guide

End-to-end install of **open-websandbox**: the gVisor nodes, the upstream
`agent-sandbox` controller + CRDs, the RWX storage, the three container images,
and the platform itself — installed via the **Helm chart** at
[`open-websandbox-platform/chart/`](../open-websandbox-platform/chart/). For the
architecture behind these components see [`architecture.md`](architecture.md);
for day-2 runbook steps see [`operations.md`](operations.md).

> **Image name placeholder.** Container image references use
> `ghcr.io/<owner>/open-websandbox-{broker,runtime,router}:<tag>`, where `<owner>`
> is the final GitHub Container Registry owner/namespace (not yet decided). In a
> Helm install you set registry/owner/tag once via chart values
> ([Configuration](#3-configuration-helm-values)) — never by hand-editing
> templates. The base manifests shipped in
> [`open-websandbox-platform/deploy/base/`](../open-websandbox-platform/deploy/base/)
> carry local/dev tags (`owui-broker:v2`, `code-standard:v5`,
> `sandbox-router-go:dev`) with `imagePullPolicy: Never`, i.e. they expect the
> images pre-loaded into containerd; the chart's values transform those to your
> published registry images.

## Prerequisites

- A Kubernetes cluster (≥1.28). Reference target: on-prem MicroK8s v1.36, Calico
  CNI, containerd, 3 control-plane + 3 worker nodes.
- **gVisor (`runsc`) installed and a `gvisor` RuntimeClass cluster-wide.**
  Sandboxes will not start without it. Follow
  [`infra/gvisor/README.md`](../infra/gvisor/README.md): stage `runsc` on each
  worker (`install-gvisor-node.sh`), activate online-safe
  (`activate-gvisor-node.sh`), and verify with the probe pod in
  [`infra/gvisor/manifests/`](../infra/gvisor/manifests/). Pinned reference
  release: `release-20260727.0`. For best performance the workers are VMs with
  `/dev/kvm` (vmx/svm) for the systrap KVM fast path.
- A **RWX `StorageClass`** for persistent workspaces. The reference cluster uses
  `cephfs` (CephFS via the hypervisor, `ReadWriteMany`). Any RWX Filesystem
  class works; set its name in chart values (`profile.persistentStorageClass`).
  (Block/RWO-only classes can only back the ephemeral profile.)
- `kubectl` (cluster-admin, to install CRDs) and **Helm 3.12+** on your
  workstation.

## 1. Install the upstream agent-sandbox controller + CRDs

The chart assumes the controller + CRDs already exist — it does **not** install
the upstream `agent-sandbox` project (a prerequisite, like gVisor and the RWX
StorageClass). Install the **pinned v0.5.3** manifest vendored in
[`open-websandbox-platform/upstream/`](../open-websandbox-platform/upstream/). It
carries the four CRDs (`agents.x-k8s.io/Sandbox`,
`extensions.agents.x-k8s.io/{SandboxTemplate,SandboxWarmPool,SandboxClaim}`) and
the controller Deployment into `agent-sandbox-system`. The vendored copy is
SHA256-recorded in
[`upstream/SHA256SUMS`](../open-websandbox-platform/upstream/SHA256SUMS) — verify
before applying:

```bash
sha256sum -c open-websandbox-platform/upstream/SHA256SUMS
kubectl apply -f open-websandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml
kubectl -n agent-sandbox-system wait deploy/agent-sandbox-controller \
  --for=condition=Available --timeout=120s
kubectl get crd | grep -E 'agents.x-k8s.io|extensions.agents.x-k8s.io'
```

> Never `latest`. Production images must be pinned by digest (spec §4); the
> upstream manifest itself is the pinned artifact here.

## 2. Build & load the images

There are three images. The chart templates default to local/dev tags with
`imagePullPolicy: Never`, so each gVisor worker's containerd must already hold
the image (or you set a registry owner/tag + `IfNotPresent` in values and push to
that registry).

```bash
# broker — Python FastAPI
docker build -t ghcr.io/<owner>/open-websandbox-broker:<tag> open-websandbox-platform/broker

# runtime — Python FastAPI + Node + toolchain + curated libs
docker build -t ghcr.io/<owner>/open-websandbox-runtime:<tag> open-websandbox-platform/runtime

# sandbox-router — Go (self-build; upstream publishes only :latest at v0.5.3,
# so it is self-built and pinned by digest in production)
docker build -t ghcr.io/<owner>/open-websandbox-router:<tag> <upstream-router-src>
```

Load each into every gVisor worker's containerd (MicroK8s):

```bash
for img in open-websandbox-broker open-websandbox-runtime open-websandbox-router; do
  docker save ghcr.io/<owner>/$img:<tag> \
    | ssh ubuntu@<worker> 'sudo microk8s.ctr images import -'
done
```

The base-image tags the chart transforms from are:

| Component | Base/local tag |
|-----------|----------------|
| broker | `owui-broker:v2` |
| runtime | `code-standard:v5` |
| router | `sandbox-router-go:dev` |

## Private registry & imagePullSecret (optional)

If your images live in a **private** registry (not pre-loaded into containerd
and not anonymously pullable), Kubernetes needs pull credentials. The chart exposes
a top-level `imagePullSecrets` value — a list of `{name: ...}` refs applied to the
broker, router, AND runtime (SandboxTemplate) pod specs.

### Recommended: declare it in Helm

1. Create the pull secret in **both** namespaces (here for GHCR):

   ```bash
   kubectl -n agent-sandbox-system create secret docker-registry regcred \
     --docker-server=ghcr.io \
     --docker-username=$OWNER \
     --docker-password=$GITHUB_PAT \
     --docker-email=you@example.com
   kubectl -n agent-sandbox-runtime create secret docker-registry regcred \
     --docker-server=ghcr.io \
     --docker-username=$OWNER \
     --docker-password=$GITHUB_PAT \
     --docker-email=you@example.com
   ```

   (Secrets are namespace-scoped, so create one per namespace.)

2. Reference it from your values file:

   ```yaml
   imagePullSecrets:
     - name: regcred
   ```

3. Install/upgrade with those values. The chart attaches the secret to the
   `owui-broker` and `sandbox-router` Deployments and to the SandboxTemplate pod
   spec, so every workload pulls as `regcred`. Because it is a Helm value, `helm
   upgrade` reconciles it (unlike a hand-patched ServiceAccount).

### Alternative: patch the ServiceAccounts

If you cannot re-run `helm upgrade`, you can instead attach the secret to the
ServiceAccounts the workloads run as (`owui-broker`, `sandbox-router` in
`agent-sandbox-system`; `default` in `agent-sandbox-runtime`). This is **not**
reconciled by `helm upgrade`, so treat it as a stop-gap:

   ```bash
   kubectl -n agent-sandbox-system patch serviceaccount owui-broker \
     -p '{"imagePullSecrets":[{"name":"regcred"}]}'
   kubectl -n agent-sandbox-system patch serviceaccount sandbox-router \
     -p '{"imagePullSecrets":[{"name":"regcred"}]}'
   kubectl -n agent-sandbox-runtime patch serviceaccount default \
     -p '{"imagePullSecrets":[{"name":"regcred"}]}'
   kubectl -n agent-sandbox-system rollout restart deploy/owui-broker deploy/sandbox-router
   # sandbox pods: recycle via the operations "Roll runtime image" procedure
   ```

> If you **pre-load** images into each gVisor worker's containerd instead
> (section 2 above) and keep `imagePullPolicy: Never`, you need no pull secret.

## 3. Configuration (Helm values)

All configuration is values in the chart. The chart renders the namespaces, the
broker Deployment + its Secret, the Go router (+ RBAC/Service/NetworkPolicy/PDB),
the `code-standard-v1` SandboxTemplate, the `code-standard-warmpool` WarmPool,
the shared RWX PVC, and the runtime-namespace ResourceQuota/LimitRange +
NetworkPolicy. A representative `my-values.yaml`:

```yaml
# open-websandbox-platform/chart/values.yaml — override these in my-values.yaml
global:
  imageRegistry: ghcr.io/<owner>      # registry + owner/namespace
  imagePullPolicy: Never              # Never = pre-loaded; IfNotPresent/Always if pushed

namespaces:
  system: agent-sandbox-system        # controller, router, broker
  runtime: agent-sandbox-runtime      # templates, pools, claims, sandboxes, pods
  create: true                        # chart creates them

runtimeClassName: gvisor              # runsc; installed cluster-wide (prereq)

images:
  broker: open-websandbox-broker:<tag>
  runtime: open-websandbox-runtime:<tag>
  router: open-websandbox-router:<tag>   # self-built (upstream ships only :latest)

broker:
  sharedSecret: "<openssl rand -hex 32>"   # -> Secret owui-broker-secret/shared-secret
  warmPool: code-standard-warmpool
  baseTemplate: code-standard-v1
  routerUrl: http://sandbox-router-svc.agent-sandbox-system:8080
  claimTimeoutSeconds: 60
  proxyTimeoutSeconds: 660

profile:
  default: persistent                 # ephemeral | persistent (deploy-fixed)
  persistentMode: per-user-pvc        # per-user-pvc | shared-subpath
  persistentStorageClass: cephfs   # RWX class (prereq)
  persistentStorage: 10Gi             # per-user PVC size
  sharedPvc: workspace-shared

idle:
  ephemeralTtlSeconds: 120            # ephemeral reap
  parkIdleSeconds: 120                # persistent park (suspend)
  reapSeconds: 604800                 # persistent reap (7d)

warmPool:
  replicas: 2                         # pre-warmed ephemeral sandboxes
  templateRef: code-standard-v1

sharedPvc:
  storageClassName: cephfs    # used only in shared-subpath mode
  size: 50Gi

quota:                                # runtime-namespace ResourceQuota
  pods: 20
  requestsCpu: 10
  limitsCpu: 30
  requestsMemory: 20Gi
  limitsMemory: 40Gi
  requestsEphemeralStorage: 20Gi
  limitsEphemeralStorage: 120Gi
  persistentVolumeClaims: 50
  requestsStorage: 200Gi

router:
  authzMode: allow-all                # allow-all (default; broker is the auth boundary) | tokenreview
  cacheEnabled: true                  # Pod-IP cache (needs cluster-wide Pods get/list/watch)
```

Each value maps 1:1 to a real knob — see the [broker env-var
reference](#broker-environment-variable-reference) below for the exact mapping
and defaults. Two settings worth calling out:

- **`profile.persistentMode`** — `per-user-pvc` (default) gives each user a
  dedicated RWX PVC via the claim's `volumeClaimTemplates`; `shared-subpath`
  mounts each user's `users/<id>/` slice of the single `workspace-shared` PVC
  (then `sharedPvc` section matters).
- **`router.authzMode: tokenreview`** — makes the Go router validate caller
  tokens via `TokenReview`. The chart applies the
  `system:auth-delegator` binding only when this is set; otherwise it stays
  `allow-all` (the broker is the auth boundary, not the router).

Runtime-side knobs (env on the sandbox pod, baked into the SandboxTemplate by
the chart): `MAX_TIMEOUT` (600), `DEFAULT_TIMEOUT` (120), `MAX_OUTPUT_BYTES`
(1 MiB), `MAX_PROCS` (256, `RLIMIT_NPROC`), `MAX_TERMINAL_SESSIONS` (8),
`RUNTIME_API_KEY` (optional WS auth, off by default).

## Production values presets (must-override keys)

The chart's defaults target a typical kubeadm-style cluster — several values
are **deliberately not safe for a different cluster**. Before any non-dev
install, set at least these in your values file:

| Key | Default (unsafe) | Set to / why |
|-----|------------------|--------------|
| `imageRegistry` | `""` | `ghcr.io` (the public registry). |
| `imageOwner` | `""` | `$OWNER` — your GHCR org/namespace. |
| `imageTag` | `v0.1.0` | A pinned tag; **pin by digest** for production. |
| `imagePullPolicy` | `Never` | `IfNotPresent` once images are pulled from a registry (not pre-loaded). |
| `broker.sharedSecret` | `dev-shared-secret-change-me` | `openssl rand -hex 32` — **must override**. Becomes the broker's `BROKER_SHARED_SECRET`. |
| `router.kubeApiServerCidr` | `10.96.0.1/32` (kubeadm; default) | ClusterIP of the `kubernetes` Service. k3s: `10.43.0.1/32`; MicroK8s: `10.152.183.1/32`. Wrong here and the router can't reach the API server. |
| `router.kubeDnsCidr` | `10.96.0.10/32` (kubeadm; default) | ClusterIP of kube-dns/CoreDNS. k3s: `10.43.0.10/32`; MicroK8s: `10.152.183.10/32`. |
| `sharedPvc.storageClass` | `cephfs` | Your RWX StorageClass. Persistent sandboxes need RWX for park/resume. |
| `networkPolicy.egress.exceptCIDRs` | RFC1918 + `169.254.0.0/16` | Confirm these cover your cluster's pod/service CIDRs so sandbox egress can't reach internal services. |

### gVisor vs. runc

gVisor (`runsc`) is the whole point of this platform — never run untrusted
agent code under plain `runc` in production. The toggle is a single key:

```yaml
sandboxTemplate:
  runtimeClassName: gvisor   # default; prod. gVisor pods land only on tainted sandbox nodes.
  # runtimeClassName: ""     # omit the field entirely => plain runc (KIND e2e / local dev only)
```

`runtimeClassName: gvisor` requires the `gvisor` RuntimeClass cluster-wide
plus the dedicated, tainted sandbox nodes. gVisor node prep (install/activate
online-safely, the RuntimeClass, and a verify probe) is documented in
[`infra/gvisor/`](../infra/gvisor/) — not repeated here.

## 4. Install the chart

```bash
helm install open-websandbox open-websandbox-platform/chart/ -f my-values.yaml
```

`helm upgrade open-websandbox open-websandbox-platform/chart/ -f my-values.yaml` to
re-apply after editing values (the chart handles namespace/secret/template
reconciliation; CRDs/controller/gVisor/RWX class are untouched — they're
prerequisites installed outside Helm).

## 5. Point Open WebUI at the broker

Open WebUI talks to the broker Service
`owui-broker.agent-sandbox-system.svc.cluster.local:8080` (ClusterIP — front it
with your existing auth reverse proxy / Gateway; do not expose the broker
directly). Configure Open WebUI to send, **per session**:

| Header / param | Required | Meaning |
|----------------|----------|---------|
| `Authorization: Bearer <shared-secret>` | yes | Matches `broker.sharedSecret` (the `BROKER_SHARED_SECRET` env). |
| `X-User-Id` | yes | Selects the user's sandbox (and, on persistent, the per-user PVC). |
| `X-Session-Id` | yes | Scopes each chat to its own workspace folder; also the terminal id. |
| `X-Persistence: persistent\|ephemeral` | no | Overrides `profile.default` for admin/testing. |

The **terminal** is the OWUI "Open Terminal" feature. The broker serves the
connection-test gate at `GET /api/config` (returns
`{"features":{"terminal":true,...}}`) and the PTY over
`WS /api/terminals/{session_id}`. The WS opening frame is
`{"type":"auth","token":<shared-secret>}`; `user_id` / `session_id` may arrive
as query params (browsers can't set WS headers). Because the broker derives the
sandbox server-side from `X-User-Id`/`X-Session-Id`, the chat's terminal and its
file/execute API always land on the **same** sandbox.

## Verify

```bash
# broker + router healthy
kubectl -n agent-sandbox-system get deploy,svc
kubectl -n agent-sandbox-system rollout status deploy/owui-broker deploy/sandbox-router

# warm pool is pre-warming
kubectl -n agent-sandbox-runtime get sandboxwarmpools
kubectl -n agent-sandbox-runtime get pods            # 2 warm pods once steady

# sanity script (claims, warm pool, pods, quota, PVCs, node pressure):
open-websandbox-platform/scripts/sandbox-status.sh
```

A successful `GET /api/config` through your auth proxy returns the features
blob; the first real request (with `X-User-Id`/`X-Session-Id`) will claim a
warm sandbox. You're live.

---

## Reference

### Broker environment-variable reference

The Helm chart's broker values render to these env vars on the `owui-broker`
Deployment. Defaults match the values shipped in
[`broker.yaml`](../open-websandbox-platform/deploy/base/broker.yaml) and the
fallbacks in [`broker/main.py`](../open-websandbox-platform/broker/main.py); leave
a Helm value unset to inherit the default.

| Helm value path | Env var | Default | Purpose |
|-----------------|---------|---------|---------|
| `broker.sharedSecret` | `BROKER_SHARED_SECRET` | *(from Secret)* | Shared bearer; empty = auth disabled (dev only). |
| `broker.warmPool` | `BROKER_WARMPOOL` | `code-standard-warmpool` | Warm pool (ephemeral profile) the broker binds claims to. |
| `namespaces.runtime` | `BROKER_RUNTIME_NS` | `agent-sandbox-runtime` | Namespace holding templates/pools/claims/sandboxes. |
| `broker.routerUrl` | `BROKER_ROUTER_URL` | `http://sandbox-router-svc.agent-sandbox-system:8080` | Where the broker proxies HTTP requests. |
| `profile.default` | `BROKER_DEFAULT_PROFILE` | `persistent` | Default profile (`ephemeral` \| `persistent`). Deploy-fixed. |
| `profile.persistentMode` | `BROKER_PERSISTENT_MODE` | `per-user-pvc` | `per-user-pvc` or `shared-subpath`. |
| `profile.persistentStorage` | `BROKER_PERSISTENT_STORAGE` | `10Gi` | Per-user PVC size (per-user-pvc mode). |
| `profile.persistentStorageClass` | `BROKER_PERSISTENT_STORAGECLASS` | `cephfs` | RWX StorageClass for persistent PVCs. |
| `idle.ephemeralTtlSeconds` | `BROKER_IDLE_TTL_SECONDS` | `120` | Ephemeral reap age — claim deleted, sandbox returns to the warm pool. |
| `idle.parkIdleSeconds` | `BROKER_PARK_IDLE_SECONDS` | `120` | Persistent **park** age — sandbox `Suspended` (pod gone, PVC kept). Cold resume ~1–6 s. |
| `idle.reapSeconds` | `BROKER_REAP_SECONDS` | `604800` (7 d) | Persistent **reap** age — claim + PVC deleted. |
| `broker.claimTimeoutSeconds` | `BROKER_CLAIM_TIMEOUT_SECONDS` | `60` | Wait for `Ready` (else HTTP 504). |
| `broker.proxyTimeoutSeconds` | `BROKER_PROXY_TIMEOUT_SECONDS` | `660` | Upstream proxy timeout (> runtime `MAX_TIMEOUT` 600 s). |
| `broker.baseTemplate` | `BROKER_BASE_TEMPLATE` | `code-standard-v1` | Base SandboxTemplate cloned for persistent per-chat sandboxes. |
| `profile.sharedPvc` | `BROKER_SHARED_PVC` | `workspace-shared` | Shared PVC name (shared-subpath mode). |
| `broker.claimPrefix` / `persistentPrefix` / `chatPrefix` | `BROKER_CLAIM_PREFIX` / `BROKER_PERSISTENT_PREFIX` / `BROKER_CHAT_PREFIX` | `owui-` / `owui-p-` / `owui-c-` | Resource-name prefixes for ephemeral claims / persistent claims / per-chat sandboxes. |
| `broker.perUserPvcPrefix` | `BROKER_PER_USER_PVC_PREFIX` | `workspace-p-` | Per-user PVC name prefix (per-user-pvc mode). |

### Router flags

The Go router's behavior is also driven from chart values (`router.*`):

- `--http-bind-address=:8080`, `--metrics-bind-address=:9090`,
  `--health-probe-bind-address=:8081`, `--cluster-domain=cluster.local`,
  `--proxy-timeout=180s`, `--upstream-max-retries=3`.
- `--cache-enabled=true` (`router.cacheEnabled`) — Pod-IP cache fast path; needs
  the cluster-wide Pods `get/list/watch` in
  [`router/rbac.yaml`](../open-websandbox-platform/deploy/base/router/rbac.yaml).
  Set false for a DNS-only, lower-privilege router.
- `--authz-mode` (`router.authzMode`) — `allow-all` by default (the broker is the
  auth boundary). `tokenreview` makes the router validate caller tokens via
  `TokenReview`; the chart then applies
  [`router/rbac-tokenreview.yaml`](../open-websandbox-platform/deploy/base/router/rbac-tokenreview.yaml)
  (`system:auth-delegator`).
