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

## 2. Install the upstream controller + CRDs

The upstream `kubernetes-sigs/agent-sandbox` manifest is vendored and SHA256-recorded
in the repo — apply the local copy, not a remote URL:

```bash
# Verify integrity (run from the repo root)
sha256sum -c agent-sandbox-platform/upstream/SHA256SUMS

# Apply CRDs FIRST (forward-compatible), then controller
kubectl apply -f agent-sandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml

# Wait for the controller
kubectl -n agent-sandbox-system wait deploy/agent-sandbox-controller \
  --for=condition=Available --timeout=120s
```

This installs image `registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.5.3` and
the `agents.x-k8s.io` / `extensions.agents.x-k8s.io` CRDs (`Sandbox`, `SandboxClaim`,
`SandboxTemplate`, `SandboxWarmPool`).

## 3. Install the chart

The chart's defaults load pre-built images from a registry. The published images live at
`ghcr.io/volkermauel/open-sandbox-{broker,runtime,router}`; the tag **`v0.1.0`** is the
current release. Pick **one** of the three install methods:

```bash
# Values shared by all methods:
COMMON="--set imageRegistry=ghcr.io \
        --set imageOwner=volkermauel \
        --set imageTag=v0.1.0 \
        --set imagePullPolicy=IfNotPresent"
```

### Option A — local chart directory *(for iterating on the repo)*

```bash
helm install open-websandbox agent-sandbox-platform/chart $COMMON
```

### Option B — published chart tarball *(verified published on the v0.1.0 Release)*

The `release.yml` workflow attaches `open-sandbox-0.1.0.tgz` to the GitHub Release:

```bash
helm install open-websandbox \
  https://github.com/volkermauel/open-websandbox/releases/download/v0.1.0/open-sandbox-0.1.0.tgz \
  $COMMON
```

### Option C — OCI registry *(canonical, what `release.yml` publishes)*

```bash
helm install open-websandbox \
  oci://ghcr.io/volkermauel/charts/open-sandbox --version 0.1.0 \
  $COMMON
```

> If `helm pull oci://…` returns `403 denied`, the package is private for your fork —
> authenticate with a PAT that has `read:packages`:
> `helm registry login ghcr.io -u <you> --password-stdin < <(echo $GHCR_PAT)`.

The chart's post-install notes print the broker URL and how to retrieve the shared secret.

## 4. Wait for the control plane + warm pool

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
  and [Release readiness](release-readiness.md) list what is *not* proven yet.

## Teardown

```bash
helm uninstall open-websandbox
kubectl delete -f agent-sandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml
kubectl delete namespace agent-sandbox-runtime agent-sandbox-system
```

Deleting the namespaces also removes the per-user PVCs — back them up first if you care
about the workspaces (see [Operations > Backup & Restore](operations.md)).
