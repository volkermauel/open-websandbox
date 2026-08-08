## Why

An earlier design explored running Open WebUI's
"Open Terminal" on **one shared host daemon + per-exec `nsjail`** — maximising
density but accepting **shared-kernel** isolation (fine for trusted internal
users). The earlier design comparison found this is
mutually exclusive with hostile-tenant-grade isolation: a single shared kernel
cannot sandbox mutually-untrusted agent-generated code.

We have since confirmed (2026-08-04) that the **Kubernetes SIG `agent-sandbox`
upstream is real** (`github.com/kubernetes-sigs/agent-sandbox`, release `v0.5.3`,
`v1beta1` CRDs `agents.x-k8s.io` / `extensions.agents.x-k8s.io`, gVisor docs and
install manifest), and **proven gVisor (`runsc`) on all three MicroK8s workers**
with zero production disruption. That removes the last infra unknown and lets us
adopt the stronger design in `AgentSandbox.md`: **one gVisor sandbox per active
session**, a small **Go broker** owning auth/quota/lifecycle, a **warm pool** to
hide cold-start, Kubernetes-native CRDs, default-deny networking, and a
ValidatingAdmissionPolicy. We trade per-session pods for gVisor-grade isolation;
the warm pool keeps warm-session ready latency low.

This change **supersedes** an earlier shared-host design. The detailed
architecture, threat model, invariants, and
GitOps layout live in **`AgentSandbox.md`**; this change scopes the phased
rollout onto our on-prem MicroK8s and records cluster-specific decisions.

## Changes

- **Runtime isolation (Phase 0 — DONE).** gVisor `runsc` (systrap platform) is
  installed and verified on `gvisor-worker-1/w2/w3`; `RuntimeClass` `gvisor`
  exists cluster-wide. Reproducible playbook: `infra/gvisor/`.
- **Controller + router (Phase 1).** Install the pinned Kubernetes SIG
  `agent-sandbox` controller + CRDs (`Sandbox`, `SandboxTemplate`,
  `SandboxWarmPool`, `SandboxClaim`) and the router in `agent-sandbox-system`,
  `ALLOW_UNAUTHENTICATED_ROUTER=false`, reachable only by the broker.
- **Runtime image (Phase 2).** Build the internal `code-standard` image
  (Python 3, shell, git, curl, jq; non-root `1000:1000`; a runtime server that
  execs commands and transfers files without touching the Kubernetes API).
  Deploy `SandboxTemplate` + `SandboxWarmPool` (`replicas: 2`).
- **Broker (Phase 3).** Go service: OIDC auth via the existing Entra reverse
  proxy, profile/quota policy, `SandboxClaim` create/watch/delete, signed
  session tokens, router proxy with header strip+inject, idle/absolute expiry
  reconciler, **stateless** restart recovery rebuilt from broker-owned claims.
  HTTP API under `/v1` (sessions, exec, files, delete); optional MCP adapter.
- **Network + admission (Phase 4).** `NetworkPolicy` default-deny in
  `agent-sandbox-runtime`; allow only router ingress, DNS, S3, policy-controlled
  egress. `ValidatingAdmissionPolicy` enforcing §4 invariants (gVisor only,
  non-root, drop caps, no host*, limits, pinned images).
- **Namespaces.** `agent-sandbox-system` (controller/router/broker/admission),
  `agent-sandbox-runtime` (templates/pools/claims/sandboxes/pods),
  `agent-sandbox-observability` (optional).
- **Durable storage.** Ephemeral `emptyDir` workspace by default; durable
  artifacts written to the internal S3-compatible store with scoped, short-lived
  credentials.

## Capabilities

### New Capabilities

- `agent-sandbox-platform` — the integrated platform: gVisor sandboxes
  provisioned one-per-active-session via the SIG controller + warm pool, fronted
  by a Go broker that owns authn/authz, quotas, claim lifecycle, and stateless
  recovery, with default-deny networking and admission policy.
- `gvisor-runtime-node` — reproducible, online-safe installation of the gVisor
  `runsc` containerd handler on snap-MicroK8s worker nodes, with a RuntimeClass
  and a controlled-CNPG-failover maintenance path. (Phase 0 — delivered.)

### Modified Capabilities

*None yet* — greenfield repo (no `openspec/specs/` baseline exists).

## Impact

- **Upstream dependency:** `kubernetes-sigs/agent-sandbox` `v0.5.3` (pinned;
  manifest vendored, digest-recorded). gVisor `runsc` `release-20260727.0`.
- **Cluster:** on-prem MicroK8s (classic snap, containerd 2.2.3, Calico CNI).
  gVisor active on all three workers; no proprietary runtime dependencies. Coexists with
  CNPG, argocd, and existing workloads in isolated namespaces.
- **Repo layout:** follows `AgentSandbox.md` §21 (`agent-sandbox-platform/`:
  upstream/, images/, broker/, deploy/, scripts/). Build location decided at
  Phase 1 (this planning repo vs a new dedicated repo).
- **Open WebUI:** the broker's `/v1` HTTP API (and optional MCP) replaces the
  open-terminal tool surface; an adapter maps it to OWUI's tool integration.
- **Risks:** per-session pod cost (mitigated by warm pool); admission-policy +
  NetworkPolicy correctness under MicroK8s/Calico; whether to dedicate+taint
  sandbox nodes (§6.2) vs run on shared workers (open — `design.md` D1); nested
  virt not enabled (systrap today; `kvm` platform is a later perf option).
