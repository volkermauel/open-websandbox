# open-sandbox

**A Kubernetes sandbox runtime that backs Open WebUI's "Open Terminal" feature.**

Each chat gets an isolated Linux sandbox running under **gVisor (`runsc`)**. An
agent (or a human in the terminal UI) can run shell commands, edit files, and
install packages — on what looks like a throwaway VM, but without a VM's blast
radius. One gVisor sandbox **per active chat**; a warm pool hides cold-start;
default-deny networking keeps sandboxes off the rest of the cluster.

No Cloudflare- or `computerd`-proprietary dependencies. The control plane rests
on the upstream [`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
controller (pinned **v0.5.3**, manifest vendored + SHA256-recorded here).

> The published-image **registry owner is not yet finalized.** This README uses
> a shell variable — `export OWNER=<your-github-org>` — and references images as
> `ghcr.io/$OWNER/open-sandbox-{broker,runtime,router}`. Never hard-code an
> owner you have not been given.

## How it works (one paragraph)

Open WebUI calls the **broker** (Python/FastAPI), which authenticates the
request, get-or-creates the right sandbox user + session via the agent-sandbox
CRDs, and reverse-proxies the in-sandbox **runtime** (Python/FastAPI on
`:8888`) through the Go **sandbox-router** (Pod-IP cache fast path). The
broker's idle reaper **parks** idle persistent sandboxes (pod gone, PVC kept)
and **reaps** truly stale ones, so there is no permanent pod per user. The
runtime exposes `POST /execute`, `GET|POST /files/*`, `GET /ports`, and an
interactive PTY over `WS /api/terminals/{id}`. See
[`docs/architecture.md`](docs/architecture.md) for the full flow and isolation
layers.

```
Open WebUI ──HTTP/WS──► owui-broker :8080 ──► sandbox-router ──► runtime :8888 (in gVisor pod)
   (Bearer + X-User-Id/X-Session-Id)            (Pod-IP cache)       /execute /files/* /ports /api/terminals
```

## Quickstart

Copy-pasteable end-to-end install. It assumes cluster-admin on a cluster that
already meets the [Prerequisites](#prerequisites). Substitute `<your-github-org>`
with the GitHub org/namespace that owns the published images.

```bash
# 1. Clone and enter the repo.
git clone <this-repo-url> open-sandbox && cd open-sandbox

# 2. Set the registry owner for the published images.
#    Images are: ghcr.io/$OWNER/open-sandbox-{broker,runtime,router}:v0.1.0
export OWNER=<your-github-org>

# 3. Create the two namespaces the chart expects (chart does not create them).
kubectl create namespace agent-sandbox-system    # broker + router (control plane)
kubectl create namespace agent-sandbox-runtime  # sandbox pods + SandboxTemplate/WarmPool

# 4. Install the agent-sandbox CRDs + controller (v0.5.3, SHA256-verified).
sha256sum -c agent-sandbox-platform/upstream/SHA256SUMS
kubectl apply -f agent-sandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml
kubectl -n agent-sandbox-system wait deploy/agent-sandbox-controller \
  --for=condition=Available --timeout=120s

# 5. Generate a strong broker shared secret.
SECRET=$(openssl rand -hex 32)

# 6. Render a values file that points the chart at your registry + secret.
#    Review/override the cluster-CIDR keys (router.*) and storageClass below —
#    the chart defaults bake MicroK8s service-CIDRs and a CephFS class.
cat > /tmp/open-sandbox-values.yaml <<EOF
imageRegistry: ghcr.io
imageOwner: ${OWNER}
imageTag: v0.1.0
imagePullPolicy: IfNotPresent

broker:
  sharedSecret: "${SECRET}"

sandboxTemplate:
  runtimeClassName: gvisor      # "" => runc (no gVisor); used by the KIND e2e suite

# --- Override these for clusters that are NOT the reference MicroK8s install ---
router:
  kubeApiServerCidr: "10.96.0.1/32"   # ClusterIP of the `kubernetes` Service; kubeadm: 10.96.0.1, k3s: 10.43.0.1
  kubeDnsCidr: "10.96.0.10/32"        # ClusterIP of kube-dns/coredns; kubeadm: 10.96.0.10, k3s: 10.43.0.10
sharedPvc:
  storageClass: cephfs           # your RWX StorageClass (or set profile.persistentStorageClass)
EOF

# 7. Install via Helm.
helm install open-sandbox agent-sandbox-platform/chart/ -f /tmp/open-sandbox-values.yaml

# 8. Wait for the control plane and the warm pool (2 pre-warmed sandboxes).
kubectl -n agent-sandbox-system rollout status deploy/owui-broker deploy/sandbox-router
kubectl -n agent-sandbox-runtime wait sandboxwarmpool/code-standard-warmpool \
  --for=jsonpath='{.status.readyReplicas}'=2 --timeout=180s \
  || kubectl -n agent-sandbox-runtime get sandboxwarmpool,pods

# 9. Verify — the broker answers /api/config with the shared secret.
kubectl -n agent-sandbox-system port-forward svc/owui-broker 8080:8080 &
TOKEN=$(kubectl -n agent-sandbox-system get secret owui-broker-secret \
  -o jsonpath='{.data.shared-secret}' | base64 -d)
curl -sS http://localhost:8080/api/config -H "Authorization: Bearer ${TOKEN}"
# expect: {"features":{"terminal":true,"notebooks":false,"desktop":false}}
```

If step 9 returns the JSON above, the broker is up, authenticating, and ready
for Open WebUI to wire in. Next: point Open WebUI at
`http://owui-broker.agent-sandbox-system.svc:8080`, sending per session
`Authorization: Bearer <shared-secret>`, `X-User-Id`, `X-Session-Id`. Full
Open WebUI wiring + header table: [`docs/deploy.md`](docs/deploy.md).

## Prerequisites

Before the Quickstart, the cluster must already have:

- Kubernetes **≥ 1.28**.
- **gVisor (`runsc`) installed and a `gvisor` RuntimeClass** cluster-wide — see
  [`infra/gvisor/`](infra/gvisor/) (online-safe node install/activate playbooks
  - the `RuntimeClass` + a verify probe).
- **An RWX `StorageClass`** (e.g. `cephfs` / CephFS `ReadWriteMany`).
  Persistent sandboxes need RWX for park/resume.
- A working RWX provisioner, Calico (or other CNI enforcing `NetworkPolicy`),
  and cluster-admin rights (to apply CRDs).

For the full prerequisites checklist, base-manifest build/load steps, broker
env-var reference, and Open WebUI wiring, see [`docs/deploy.md`](docs/deploy.md).

## Documentation

| Doc | What's in it |
|-----|--------------|
| [`docs/architecture.md`](docs/architecture.md) | broker↔router↔runtime↔controller flow (with diagram), per-chat lifecycle (warm → claim → park/suspend → reap), ephemeral vs. persistent workspaces, four isolation layers. |
| [`docs/deploy.md`](docs/deploy.md) | Full prerequisites + install: gVisor nodes, controller + CRDs, RWX storage, namespaces, private-registry image pull, building/loading 3 images, broker shared-secret, Open WebUI wiring, broker env-var table, prod values presets. |
| [`docs/operations.md`](docs/operations.md) | Runbook: warm-pool tuning, idle park/reap policy, ResourceQuota/LimitRange limits, Backup & Restore (per-user PVCs), troubleshooting (reaper stuck, WS-proxy 504, PVC Pending, sandbox NotReady, silent partial-outage), rolling runtime image, upgrade/rollback. |
| [`docs/release-readiness.md`](docs/release-readiness.md) | What is and isn't proven for production; open risks. |
| [`infra/gvisor/`](infra/gvisor/) | Online-safe gVisor node install/activate playbooks + `RuntimeClass` + probe. |
| [`openspec/`](openspec/) | OpenSpec change [`adopt-agent-sandbox`](openspec/changes/adopt-agent-sandbox/) — design + spec deltas. |
| [`AgentSandbox.md`](AgentSandbox.md) | Source-analysis of the Open WebUI open-terminal AgentSandbox design (architecture, threat model, mandatory invariants, manifests). |
| [`CHANGELOG.md`](CHANGELOG.md) | Release history (Keep a Changelog). |

## Reference deployment

The reference is on-prem MicroK8s v1.36, 3 control-plane + 3 worker nodes,
Calico CNI, `cephfs` RWX storage. The chart's defaults reproduce that
environment — **override `router.kubeApiServerCidr` / `kubeDnsCidr` and the
storage class for any other cluster.**

## Contributing & status

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The v0.1.0 release is functional and
unit-covered, but **not battle-tested** — read
[`docs/release-readiness.md`](docs/release-readiness.md) before relying on it.
