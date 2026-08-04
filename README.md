# open-webui-terminal2cloudflarecompute

Planning + infrastructure repo: adopting **`kubernetes-sigs/agent-sandbox`** as the
runtime for Open WebUI's "Open Terminal" feature — gVisor-isolated sandboxes on an
on-prem MicroK8s cluster. No Cloudflare-proprietary dependencies; hostile-tenant-grade
isolation via gVisor (runsc) instead of the earlier shared-`computerd` design.

## Status

- **Phase 0 — gVisor platform** ✅ `runsc` (systrap) live on all 3 worker nodes;
  `RuntimeClass/gvisor` cluster-wide. See [`infra/gvisor/`](infra/gvisor/).
- **Phase 1 — controller + router + hardening** ✅ `agent-sandbox-controller` v0.5.3
  - 4 CRDs, Go `sandbox-router` (2 replicas), NetworkPolicies, ResourceQuota/LimitRange.
- **Phase 2+** — runtime image, broker, OWUI integration (pending).

## Layout

| Path | Contents |
|------|----------|
| `openspec/` | OpenSpec project + change proposals. Active: `changes/adopt-agent-sandbox/`. |
| `agent-sandbox-platform/` | Vendored upstream manifest (`upstream/`, SHA-pinned) + `deploy/base/` (router, NP, quota). |
| `infra/gvisor/` | Idempotent node playbooks: `install-gvisor-node.sh` (stage, inert) + `activate-gvisor-node.sh` (online-safe restart). |
| `research/` | Source-analysis reports (computer internals, OWUI contract, portability, AgentSandbox comparison). |
| `AgentSandbox.md` | Original design doc the adoption is based on. |

## Working with the change

```bash
openspec validate adopt-agent-sandbox        # validate the active change
kubectl get sandbox -A                       # CRD is live
```

Cluster: MicroK8s v1.36, 3 CP + 3 workers (`gvisor-worker-1..3`), Calico CNI, containerd.
