# Design — adopt-agent-sandbox

An ADR-style record of the **cluster-specific** decisions for adopting
`kubernetes-sigs/agent-sandbox` as the Open WebUI "Open Terminal" sandbox
runtime, superseding an earlier shared-host sandboxing direction.

The authoritative, detailed design lives in **[`AgentSandbox.md`](../../../AgentSandbox.md)**
(architecture §5, threat model §2, mandatory invariants §4, manifests §6–§17,
failure behavior §20, acceptance §24). **This document records only** the
decisions and rationale specific to that pivot onto our on-prem MicroK8s.
Where this document is silent, `AgentSandbox.md` applies. See also the change
[`proposal.md`](proposal.md).

Each decision below is written as an ADR with **Context / Decision /
Consequences** and a status. Statuses: **[ACCEPTED]**, **[NEEDS DECISION]**.

---

## ADR-1 — Runtime: gVisor, one pod per active session

### Context

The earlier shared-host design maximized density — one shared daemon, per-exec
`nsjail` — but isolation was **shared-kernel**: a kernel or `nsjail` bypass
exposes every co-resident session. That is acceptable only when callers are
trusted. The target workload is agent-generated code (semi-untrusted), so
[`AgentSandbox.md`](../../../AgentSandbox.md) §2 requires per-session
process/network/host isolation. gVisor (userspace kernel, filtered syscalls)
plus a pod per active session plus default-deny networking plus admission
policy delivers that, trading density (per-session pods vs one shared daemon)
for isolation — acceptable once a warm pool hides cold-start. Two unknowns
blocked the pivot and are now resolved: the upstream is real
(`kubernetes-sigs/agent-sandbox` v0.5.3), and gVisor `runsc` is proven on our
MicroK8s workers (online-safe, zero production disruption — see
[`infra/gvisor/`](../../../infra/gvisor/)).

### Decision

Adopt `agent-sandbox` + gVisor. One `runsc` pod per active chat; warm pool to
hide cold-start; default-deny networking + admission policy enforce §4
invariants.

### Consequences

- **(+)** Strong per-session isolation; untrusted agent code is acceptable.
- **(+)** No permanent pod per user — the idle reaper parks/reaps (see ADR-D7),
  so steady-state pod count is low.
- **(−)** Two new operational surfaces: gVisor node prep (capability
  `gvisor-runtime-node`) and the agent-sandbox controller.
- **(−)** Per-session pod cost vs one shared daemon; mitigated by warm pool.

**Status: [ACCEPTED]** — supersedes the earlier shared-host design.

---

## ADR-D1 — Dedicated + tainted sandbox nodes vs. co-tenancy with CNPG/argocd

### **[NEEDS DECISION]**

### Context

[`AgentSandbox.md`](../../../AgentSandbox.md) §6.2 wants **dedicated**,
`NoSchedule`-tainted sandbox nodes (≥1, preferably 2 for maintenance/HA),
labelled `workload.open-websandbox.local/type=agent-sandbox`, so untrusted sandbox pods
never co-reside with cluster-critical or trusted workloads and so a
noisy/compromised sandbox cannot starve neighbours. The target cluster already
runs **CNPG** (Postgres), **ArgoCD**, and other stateful/management workloads on
shared workers, and some nodes expose `/dev/kvm` (vmx/svm) for VM workloads.
Co-locating sandboxes on those nodes re-introduces exactly the co-tenancy risk
gVisor is meant to remove; but dedicating 1–2 nodes is an added capacity/cost
commitment and removes them from the CNPG/argocd pool. There is also an
operational coupling: node maintenance on a shared worker that hosts the CNPG
**primary** requires a controlled `kubectl cnpg promote` switchover first
(proven on `w1`; part of capability `gvisor-runtime-node`).

### Decision

**OPEN — maintainer to decide before Phase 1 apply.** Recommended option below.

### Recommended option + rationale

**Dedicate + taint sandbox nodes** (follow §6.2 as written): label +
`NoSchedule`-taint 1 node (single-cluster) / 2 nodes (HA + maintenance) as
`workload.open-websandbox.local/type=agent-sandbox`, install/activate gVisor there, and
bind the `gvisor` `RuntimeClass` node selector + toleration to them.

- gVisor's value proposition is isolation of *untrusted* code; co-locating on
  CNPG/argocd nodes reintroduces the blast radius the platform exists to remove
  — even with gVisor, `runsc` escape, node resource exhaustion, or a shared
  kubelet/CNI compromise would then cross into trusted workloads.
- Dedicated nodes give a clean blast-radius and noisy-neighbour boundary and
  make sandbox maintenance (gVisor upgrades, node drain) **independent** of CNPG
  primary switchovers.
- Cost is bounded: the idle reaper + warm pool keep steady-state pod count low
  (no permanent pod per user), so 1–2 modest nodes suffice.

**Trade-off to accept:** those nodes leave the general-purpose pool; size the
remaining cluster for CNPG/argocd accordingly.

### Alternative considered

**Co-tenant on shared workers** (lower node cost) — only acceptable if every
co-resident workload is itself trusted AND `runsc` escape is accepted as
residual risk. Explicitly **rejected as the default** because it negates the
platform's core guarantee. Revisit only under hard node-budget constraints, and
then only with a documented residual-risk acceptance.

### Consequences (pending the decision)

- Gates Phase 1 apply (see [Open questions](#open-questions)).
- Drives the `gvisor-runtime-node` capability and the node-prep runbooks under
  [`infra/gvisor/`](../../../infra/gvisor/).
- If dedicated nodes are chosen, CNPG/argocd lose 1–2 workers; re-plan capacity.

---

## ADR-D4 — Broker language: FastAPI shipped, Go target deferred

### Context

[`AgentSandbox.md`](../../../AgentSandbox.md) §10.2 prefers a small **Go**
broker: static binary, first-class k8s client libraries, low idle memory,
straightforward streaming/cancellation. It explicitly allows a **FastAPI PoC**
to de-risk Phase 3.

### Decision

Ship the **FastAPI** broker for v0.1.0 (the de-risk PoC succeeded well enough to
become the implementation); defer the Go reimplementation. API and secret
behaviour must stay identical if/when a Go broker replaces it.

### Consequences

- **(+)** Faster to iterate; matches the team's Python tooling; 100% unit/branch
  coverage achieved.
- **(−)** Higher idle memory than a Go binary; Go's streaming/cancellation
  ergonomics forgone for now.
- **(−)** A future Go rewrite must preserve the open-terminal contract
  (`/execute`, `/files/*`, `/ports`, `/api/terminals/{id}`, `/api/config`) and
  the shared-secret auth exactly.

**Status: [ACCEPTED]** — revisit during production hardening.

---

## ADR-D5 — Upstream version pin

### Context

Reproducibility and supply-chain integrity require a fixed, verifiable upstream.

### Decision

Pin `kubernetes-sigs/agent-sandbox` at **v0.5.3**. Vendor the install manifest
into [`agent-sandbox-platform/upstream/`](../../../agent-sandbox-platform/)
with a recorded **SHA256**. Never reference `latest` in production (§4.16).

### Consequences

- **(+)** Reproducible, tamper-evident installs and upgrades.
- **(−)** Security/feature fixes require a deliberate re-vendor + SHA256
  re-record + re-test; never an accidental bump.

**Status: [ACCEPTED]**

---

## ADR-D6 — Authentication & identity path

### Context

Reuse the existing Entra (OIDC) reverse proxy for SSO rather than building auth
into the broker; the broker must never trust a request that bypasses that path.

### Decision

Caller identity is trusted **only** when the request arrives through the
configured auth proxy (identity conveyed via `X-User-Id` / `X-Session-Id`
headers); direct broker access is denied by NetworkPolicy (§12.1, §13.4). The
wire-auth between Open WebUI and the broker is a **shared secret**
(`BROKER_SHARED_SECRET`, `Authorization: Bearer`). `X-Persistence` remains an
optional admin override only.

### Consequences

- **(+)** No new identity provider to operate; SSO lives where it already does.
- **(−)** Correctness depends entirely on NetworkPolicy + proxy config. If either
  drifts so the broker is directly reachable, identity headers can be spoofed —
  treat "broker reachable without the proxy" as a security incident.

**Status: [ACCEPTED]**

---

## ADR-D7 — Persistent workspace by default; deploy-selected backing

### Context

`/workspace` MUST survive pod/image rollouts — an ephemeral (`emptyDir`) default
destroyed user data whenever a pod was deleted (image upgrade, node drain, OOM,
park), observed in production. Open WebUI cannot send arbitrary per-request
headers, so the profile must be fixed at deploy time
(`BROKER_DEFAULT_PROFILE=persistent`).

### Decision

Default to **persistent** workspaces, deploy-fixed via
`BROKER_DEFAULT_PROFILE=persistent`; `X-Persistence` stays as an optional admin
override. Two persistent backends, selected by `BROKER_PERSISTENT_MODE`:

- **`per-user-pvc` (default)** — per-user RWX PVC (`workspace-p-<hash>`) mounted
  at `/workspace`; strongest cross-user isolation, one PVC per user.
- **`shared-subpath`** — one shared RWX PVC with per-session subdirectories
  (named by `sha256` hash); cheaper, weaker cross-user isolation.

### Consequences

- **(+)** No silent data loss on rollout/drain/OOM/park.
- **(+)** `per-user-pvc` gives clean cross-user isolation; `shared-subpath` cuts
  PVC count.
- **(−)** Storage cost scales with users (`per-user-pvc`) or carries cross-user
  path risk (`shared-subpath`, mitigated by subdir hashing + the runtime path
  guard).
- **(−)** Quota (`persistentVolumeClaims: 50`) bounds simultaneous persistent
  users; tune via reaper policy.

**Status: [ACCEPTED]**

---

## Explicitly NOT done here (deferred / non-goals)

Per [`AgentSandbox.md`](../../../AgentSandbox.md) §3: no permanent desktops/VMs,
no Windows, no GPU, no uncontrolled internet egress, no in-sandbox Kubernetes
API, no arbitrary inbound ports. Interactive PTY / notebooks / port-proxy
(open-terminal extras) are out of scope for v1 — same as the earlier design's
Phase 5.

## Open questions

*Resolve before Phase 1 apply.*

1. **D1: dedicate+taint sandbox nodes, or shared workers first?** —
   **`[NEEDS DECISION]`**; recommendation above.
2. Build location: this planning repo (`agent-sandbox-platform/` subdir) vs. a
   new dedicated repo?
3. Apply target: production cluster now, or a dev overlay/namespace first?
4. Persistent backing selection (D7) — confirm `per-user-pvc` for Phase 3.
