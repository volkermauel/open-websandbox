# Quick start

End-to-end install of **open-websandbox** for a cluster-admin, from zero to a working
sandbox and a smoke test. Takes ~10 minutes once the prerequisites are in place.

> This is the fast path. For the full prerequisites matrix, private-registry image
> pull, building/loading the three images yourself, broker env-var reference, and
> production values presets, see the [Deployment guide](deploy.md).

## What you get

- The upstream **`agent-sandbox` controller + CRDs** (v0.5.3) in `agent-sandbox-system`.
- The **broker** (`owui-broker`), **sandbox-router** (`sandbox-router`), and a warm pool
  of **runtime** pods in `agent-sandbox-runtime` — all under gVisor.
- A `SandboxTemplate` (`code-standard-v1`) and a `SandboxWarmPool`
  (`code-standard-warmpool`, 2 replicas) so the first chat doesn't pay cold-start.

## 0. Prerequisites

| Need | Check |
|------|-------|
| Kubernetes **>= 1.28**, cluster-admin rights | `kubectl auth can-i '*' '*' --all-namespaces` |
| **gVisor `RuntimeClass`** cluster-wide | `kubectl get runtimeclass gvisor` (if missing: [`infra/gvisor/`](../infra/gvisor/)) |
| **RWX `StorageClass`** (persistent workspaces) | `kubectl get sc` — need a `ReadWriteMany` class (e.g. CephFS) |
| CNI that enforces `NetworkPolicy` | Calico / Cilium / Weave — most managed CNIs qualify |
| `helm` >= 3.12, `kubectl` | `helm version`, `kubectl version --client` |

No gVisor yet? It's an online-safe, rolling install — see
[`infra/gvisor/README.md`](../infra/gvisor/README.md). (To try the platform on a gVisor-less
node, set `sandboxTemplate.runtimeClassName=""` for runc — but that disables the
strongest isolation layer; see [Architecture > Isolation layers](architecture.md#isolation-layers).)

## 1. Create the namespaces

The chart deliberately does **not** create them (so quota/RBAC stay under cluster-admin
control):

```bash
kubectl create namespace agent-sandbox-system agent-sandbox-runtime
```

## 2. (Optional) Verify the vendored upstream manifest

Since [#39](https://github.com/volkermauel/open-websandbox/pull/39), the chart installs
the upstream `agent-sandbox` controller + CRDs for you (`upstream.deploy: true` by
default; the four `agents.x-k8s.io` / `extensions.agents.x-k8s.io` CRDs ship in
`chart/crds/` and are applied before the chart's templates). There is **no separate
manual `kubectl apply` step** — `helm install` in the next section brings up the
whole platform, including the controller (image
`registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.5.3`).

The manifest the chart renders from is vendored and SHA256-recorded in the repo.
You can verify its integrity before installing (run from the repo root):

```bash
sha256sum -c open-websandbox-platform/upstream/SHA256SUMS
```

> **Managing the controller yourself?** If the upstream controller already runs
> cluster-wide, install with `--set upstream.deploy=false` (and add `--skip-crds` if
> the CRDs are already present). In that case apply the vendored manifest yourself:
> `kubectl apply -f open-websandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml`.

## 3. Install the chart

!!! warning "Pre-release — artifacts are not published yet"

    **No GitHub Release has been cut yet.** The pre-built images
    (`ghcr.io/volkermauel/open-websandbox-{broker,runtime,router}:v0.1.0`), the OCI chart
    (`oci://ghcr.io/volkermauel/charts/open-websandbox --version 0.1.0`), and the release
    tarball (`…/releases/download/v0.1.0/open-websandbox-0.1.0.tgz`) are all produced by
    the [`release.yml`](../.github/workflows/release.yml) workflow **on the first
    `v0.1.0` git tag**. Until that tag exists those references `404`, and any install
    that pulls them ends in `ImagePullBackOff`.

    **Until the first release, use Option A (build from source) below** — it builds
    the three images locally and installs from your checkout with no registry pull.
    Options B and C (the OCI chart and the release tarball) are the intended
    post-release install paths, kept here verbatim; they become valid the moment
    `v0.1.0` is tagged.

The chart ships three images — `open-websandbox-broker`, `open-websandbox-runtime`, and
`open-websandbox-router` (the last is self-built from upstream `kubernetes-sigs/agent-sandbox`
v0.5.3; see [`release.yml`](../.github/workflows/release.yml)). Match the install path
to your situation:

### Option A — build from source *(primary path until v0.1.0 is released)*

Build the three images and load them into each gVisor worker, then install from your
local checkout. The chart defaults to local/dev tags with `imagePullPolicy: Never`,
so **no registry pull happens** — Kubernetes uses the images you loaded:

```bash
# 1. Build + load the 3 images (broker, runtime, router). Exact `docker build` +
#    `kind load` / `microk8s.ctr` commands are in the Deployment guide, §2:
#       docs/deploy.md  →  "Build & load the images"
# 2. Install from the local chart (default values = pre-loaded images, no GHCR pull):
helm install open-websandbox open-websandbox-platform/chart
```

See [Deployment guide §2 — Build & load the images](deploy.md#2-build-load-the-images)
for the build/load commands, and [§3 Configuration](deploy.md#3-configuration-helm-values)
to push to your own registry (`--set imageRegistry` / `imageOwner` + `IfNotPresent`)
instead of pre-loading.

The two **post-release** methods below pull the published images from GHCR. They share
these values:

```bash
# Values shared by the post-release methods (B and C) — pull published images from GHCR:
COMMON="--set imageRegistry=ghcr.io \
        --set imageOwner=volkermauel \
        --set imageTag=v0.1.0 \
        --set imagePullPolicy=IfNotPresent"
```

### Option B — published chart tarball *(available once the v0.1.0 Release is published)*

Once `release.yml` runs on the `v0.1.0` tag it attaches `open-websandbox-0.1.0.tgz` to the
GitHub Release:

```bash
helm install open-websandbox \
  https://github.com/volkermauel/open-websandbox/releases/download/v0.1.0/open-websandbox-0.1.0.tgz \
  $COMMON
```

### Option C — OCI registry *(canonical post-release path — what `release.yml` publishes)*

Once released, the chart is also pushed to GHCR by `release.yml`:

```bash
helm install open-websandbox \
  oci://ghcr.io/volkermauel/charts/open-websandbox --version 0.1.0 \
  $COMMON
```

> If `helm pull oci://…` returns `403 denied`, the package is private for your fork —
> authenticate with a PAT that has `read:packages`:
> `helm registry login ghcr.io -u <you> --password-stdin < <(echo $GHCR_PAT)`.

The chart's post-install notes print the broker URL and how to retrieve the shared secret.

## 4. Wait for the control plane + warm pool, then verify

```bash
# Broker + router
kubectl -n agent-sandbox-system wait deploy/owui-broker deploy/sandbox-router \
  --for=condition=Available --timeout=180s

# Warm pool pre-warming (expect readyReplicas == 2)
kubectl -n agent-sandbox-runtime wait sandboxwarmpool/code-standard-warmpool \
  --for=jsonpath='{.status.readyReplicas}'=2 --timeout=300s
```

If the warm pool is stuck at `0`, the usual cause is no schedulable gVisor node — re-run
`infra/gvisor/activate-gvisor-node.sh` and check `kubectl get nodes -o wide
-l sandbox-runtime=gvisor`. See [Operations > Troubleshooting](operations.md).

### Verify the install

Confirm every component the chart brought up is healthy — the upstream
controller + CRDs, the broker, the router, and the warm pool of runtime pods:

```bash
# Upstream agent-sandbox controller (installed by the chart, upstream.deploy=true)
kubectl -n agent-sandbox-system get deploy/agent-sandbox-controller

# The four agents.x-k8s.io / extensions.agents.x-k8s.io CRDs (from chart/crds/)
kubectl get crd | grep -E 'agents.x-k8s.io|extensions.agents.x-k8s.io'

# Control plane pods (broker + router) — all Running/Ready
kubectl -n agent-sandbox-system get pods -l app.kubernetes.io/part-of=open-websandbox

# Warm pool — 2 pre-warmed runtime pods under gVisor (runsc)
kubectl -n agent-sandbox-runtime get pods
kubectl -n agent-sandbox-runtime get sandboxwarmpool
```

Expected: the controller, broker, and router Deployments are `Available`; the four
CRDs are listed; and `code-standard-warmpool` shows `2` ready replicas. If anything is
missing, re-check `helm status open-websandbox` and the [Operations > Troubleshooting](operations.md)
guide.

## 5. Wire up Open WebUI

Point Open WebUI's terminal backend at the broker and give it the shared secret.

```bash
# Broker URL (in-cluster):
#   http://owui-broker.agent-sandbox-system.svc:8080

# Retrieve the auto-generated shared secret (leave broker.sharedSecret unset to auto-gen):
kubectl -n agent-sandbox-system get secret owui-broker-secret \
  -o jsonpath='{.data.shared-secret}' | base64 -d ; echo
```

Configure Open WebUI to send, on every request:

- header `Authorization: Bearer <shared-secret>`,
- header `X-User-Id: <stable-user-id>`,
- header `X-Session-Id: <stable-session-id>`.

For reproducible deploys / CI, set the secret explicitly instead:
`--set broker.sharedSecret="$(openssl rand -hex 24)"` (the chart refuses the placeholder
`dev-shared-secret-change-me`).

## 6. Smoke test

Port-forward the broker and exercise the API end to end:

```bash
# Terminal 1 — keep this running
kubectl -n agent-sandbox-system port-forward svc/owui-broker 8080:8080
```

```bash
# Terminal 2
TOKEN=$(kubectl -n agent-sandbox-system get secret owui-broker-secret \
  -o jsonpath='{.data.shared-secret}' | base64 -d)

# Run a command in a fresh sandbox
curl -sS http://localhost:8080/execute \
  -H "Authorization: Bearer $TOKEN" \
  -H 'X-User-Id: qs' -H 'X-Session-Id: qs1' \
  -H 'Content-Type: application/json' \
  -d '{"command":"echo hello from $(hostname) && uname -srm"}'

# (persistent mode) write a file, then in a second call prove it survived
curl -sS http://localhost:8080/execute \
  -H "Authorization: Bearer $TOKEN" \
  -H 'X-User-Id: qs' -H 'X-Session-Id: qs2' \
  -H 'Content-Type: application/json' \
  -d '{"command":"echo persist > /workspace/proof.txt"}'
```

Watch a sandbox get claimed and (after ~2 min idle) parked/reaped:

```bash
kubectl -n agent-sandbox-system logs -f deploy/owui-broker | \
  grep -iE 'claim|park|reap|suspend|operatingMode'
```

## Next steps

- **Understand the lifecycle** (warm → claim → park/suspend → reap) and the isolation
  model: [Architecture](architecture.md).
- **Tune the warm pool, idle policy, and quotas; back up per-user PVCs; roll the runtime
  image:** [Operations runbook](operations.md).
- **Go to production:** the [Production-readiness checklist](production-readiness-checklist.md)
  lists what is *not* proven yet.

## Teardown

```bash
helm uninstall open-websandbox
# Only if you applied the upstream manifest yourself (--set upstream.deploy=false);
# otherwise 'helm uninstall' already removes the controller + CRDs it installed:
# kubectl delete -f open-websandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml
kubectl delete namespace agent-sandbox-runtime agent-sandbox-system
```

Deleting the namespaces also removes the per-user PVCs — back them up first if you care
about the workspaces (see [Operations > Backup & Restore](operations.md)).
