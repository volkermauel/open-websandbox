# open-sandbox

**A Kubernetes runtime that backs Open WebUI's "Open Terminal" feature.**

Each chat gets an isolated Linux sandbox running under **gVisor (`runsc`)**. An
agent (or a human in the terminal UI) can run shell commands, edit files, and
install packages — on what looks like a throwaway VM, but without that VM's
blast radius. One gVisor sandbox **per active chat**, a warm pool hides
cold-start, and default-deny networking keeps sandboxes off the cluster.

No Cloudflare-proprietary dependencies. The runtime rests on the upstream
[`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
controller (pinned **v0.5.3**, manifest vendored + SHA256-recorded here).

## How it works (one paragraph)

Open WebUI calls the **broker** (Python FastAPI), which authenticates the
request, get-or-creates the right sandbox for the user+session via agent-sandbox
CRDs, and reverse-proxies to the in-sandbox **runtime** (Python FastAPI on
`:8888`) through the Go **sandbox-router** (Pod-IP cache fast path). The broker
runs an idle reaper that **parks** idle persistent sandboxes (pod gone, PVC
kept) and **reaps** truly stale ones, so there's no permanent pod per user. The
runtime exposes `POST /execute`, `GET/POST /files/*`, `GET /ports`, and a PTY
over `WS /api/terminals/{id}`. See [`docs/architecture.md`](docs/architecture.md)
for the full flow + isolation layers.

```
Open WebUI ──► broker ──HTTP──► sandbox-router ──► sandbox pod (gVisor + runtime :8888)
              (auth,     (Pod-IP fast path)            /execute  /files/*  /api/terminals (PTY)
               resolve,                                 ▲
               reap)                                    │ WS terminal dials Pod IP directly
```

## Quickstart

> Prereqs: a cluster (≥1.28) with **gVisor `runsc` installed and a `gvisor`
> RuntimeClass** cluster-wide (see [`infra/gvisor/`](infra/gvisor/)), an **RWX
> StorageClass** for persistent workspaces (reference: `cephfs`), and
> `kubectl` cluster-admin.

```bash
# 1. controller + CRDs (pinned v0.5.3, SHA256-verified)
sha256sum -c agent-sandbox-platform/upstream/SHA256SUMS
kubectl apply -f agent-sandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml

# 2. namespaces (or set namespaces.create: true in values below)
kubectl create namespace agent-sandbox-system      # controller, router, broker
kubectl create namespace agent-sandbox-runtime     # templates, pools, claims, sandboxes, pods

# 3. build & load the three images (broker, runtime, router) into each gVisor worker

# 4. install the platform via Helm (config in my-values.yaml: images,
#    broker secret/env, runtimeClassName, warm pool, PVC/storageClass, idle TTLs)
helm install open-sandbox agent-sandbox-platform/chart/ -f my-values.yaml

# 5. wire Open WebUI → broker, sending per session:
#      Authorization: Bearer <shared-secret>, X-User-Id, X-Session-Id
kubectl -n agent-sandbox-runtime get sandboxwarmpools   # 2 warm pods once steady
```

Image references use the placeholder `ghcr.io/<owner>/open-sandbox-{broker,runtime,router}`
(owner not finalized). Full install, build/load, config, and OWUI wiring steps:
[`docs/deploy.md`](docs/deploy.md).

## Documentation

| Doc | What's in it |
|-----|--------------|
| [`docs/architecture.md`](docs/architecture.md) | broker↔router↔runtime↔controller flow (with diagram), per-chat lifecycle (warm → claim → park/suspend → reap), ephemeral vs. persistent workspaces, the four isolation layers. |
| [`docs/deploy.md`](docs/deploy.md) | Full prerequisites + install: gVisor nodes, controller + CRDs, RWX storage, namespaces, manifests, building/loading the 3 images, the broker shared-secret, Open WebUI wiring, and the broker env-var table. |
| [`docs/operations.md`](docs/operations.md) | Runbook: warm-pool tuning, idle park/reap policy, ResourceQuota/LimitRange limits, troubleshooting (sandbox NotReady, terminal 429, DNS/egress, `imagePullPolicy: Never`), rolling the runtime image, upgrade/rollback. |
| [`infra/gvisor/`](infra/gvisor/) | Online-safe gVisor node install/activate playbooks + `RuntimeClass` + probe. |
| [`openspec/`](openspec/) | OpenSpec change proposals. Active: [`changes/adopt-agent-sandbox/`](openspec/changes/adopt-agent-sandbox/) — the design decision log. |
| [`AgentSandbox.md`](AgentSandbox.md) | Authoritative platform spec (architecture, threat model, invariants, manifests). |
| [`research/`](research/) | Source-analysis reports (OWUI open-terminal contract, AgentSandbox comparison, portability). |

## Status

- **gVisor platform** — `runsc` (systrap) live on all worker nodes;
  `RuntimeClass/gvisor` cluster-wide. ([`infra/gvisor/`](infra/gvisor/))
- **Controller + router + hardening** — `agent-sandbox` v0.5.3 (4 CRDs), Go
  `sandbox-router` (2 replicas), broker, NetworkPolicies, ResourceQuota /
  LimitRange.
- **Runtime + broker + OWUI integration** — implemented
  ([`agent-sandbox-platform/broker/`](agent-sandbox-platform/broker),
  [`agent-sandbox-platform/runtime/`](agent-sandbox-platform/runtime)).

Reference cluster: on-prem MicroK8s v1.36, 3 control-plane + 3 workers, Calico
CNI, containerd, `cephfs` RWX storage.

## Working with the change

```bash
openspec validate adopt-agent-sandbox        # validate the active OpenSpec change
agent-sandbox-platform/scripts/sandbox-status.sh   # one-glance platform health
kubectl get sandbox -A                       # CRD is live
```
