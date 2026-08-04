# Design — adopt-agent-sandbox

The authoritative, detailed design is **`AgentSandbox.md`** (architecture §5,
threat model §2, mandatory invariants §4, manifests §6–§17, failure behavior §20,
acceptance §24). This document records **only** the cluster-specific decisions
and the rationale for the pivot from the computerd direction. Where this document
is silent, `AgentSandbox.md` applies.

## Why we pivoted from `use-computerd-as-runtime`

The computerd design (`use-computerd-as-runtime`) maximises density — one shared
`computerd`, per-exec `nsjail` — but isolation is **shared-kernel**: a kernel or
nsjail bypass exposes every co-resident session. That is acceptable only when all
callers are trusted. The target workload is **agent-generated code** (semi- to
un-trusted), for which `AgentSandbox.md` §2 requires process/network/host
isolation per session. gVisor (userspace kernel, filtered syscalls) plus a pod
per active session plus default-deny networking plus an admission policy provides
that, and the trade (per-session pods vs one shared daemon) is the right one once
the warm pool hides cold-start. Two unknowns blocked the pivot and are now
resolved: the **upstream is real** (`kubernetes-sigs/agent-sandbox` v0.5.3), and
**gVisor runs on our MicroK8s** (proven, online-safe, on all workers).

## Cluster-specific decisions

- **D1 — Dedicated vs shared sandbox nodes (OPEN).** `AgentSandbox.md` §6.2 wants
  dedicated, `NoSchedule`-tainted worker(s) labelled
  `workload.open-websandbox.local/type=agent-sandbox`. Today gVisor is active on **all
  three** shared workers (no taint). Options: (a) dedicate+taint 1–2 workers
  (stronger: sandbox pods can't co-locate with CNPG/argocd/etc.; needs the
  RuntimeClass Variant B nodeSelector+toleration from `infra/gvisor/manifests/`);
  (b) run on shared workers initially (simpler; weaker node-level separation).
  Decided at task 1.1. Until then the `gvisor` RuntimeClass is Variant A.

- **D2 — gVisor platform = `systrap`.** The the hypervisor VMs expose no nested
  virtualisation (`/dev/kvm` absent; no vmx/svm). systrap needs none and is
  proven on-cluster. The `kvm` platform is a later performance option if nested
  virt is enabled (also unlocks Kata Containers, §3 non-goal for v1).

- **D3 — MicroK8s classic snap; CNPG coexistence.** The runsc handler is injected
  through `containerd-template.toml` (the rendered `containerd.toml` is
  regenerated on restart). The classic snap is unconfined, so `/usr/local/bin`
  is on containerd's PATH. The agent-sandbox namespaces are isolated from CNPG,
  argocd, and other workloads; node maintenance that touches a CNPG **primary**
  requires a controlled `kubectl cnpg promote` switchover first (done for `w1`).
  See capability `gvisor-runtime-node`.

- **D4 — Broker in Go.** Per §10.2: small static binary, first-class k8s client
  libs, low idle memory, straightforward streaming/cancellation. A FastAPI PoC is
  acceptable to de-risk Phase 3 if needed, but the API/secret behaviour must match.

- **D5 — Upstream pin.** Baseline `agent-sandbox` `v0.5.3`; the install manifest
  is vendored and SHA256-recorded into `upstream/`. Confirm the latest stable at
  task 1.2 and pin deliberately (never `latest` in production — §4.16).

- **D6 — Auth via the existing Entra OIDC reverse proxy.** The broker trusts
  identity headers only when the request arrives through the configured auth
  proxy; direct broker access is blocked by NetworkPolicy (§12.1, §13.4).

- **D7 — Ephemeral workspace, durable S3.** Default profile uses `emptyDir`
  (no per-session PVC); durable artifacts go to the internal S3-compatible store
  with scoped, short-lived credentials (§13.5, §14). Which S3 backend (existing
  Ceph RGW / MinIO / rustfs) is confirmed at Phase 3.

- **D8 — One sandbox per active session, not per user (§1).** Users consume a
  sandbox only while a session is active; on session end the claimed sandbox is
  destroyed and the warm pool builds a clean replacement. No permanent pod per
  user. This is the core density/isolation tradeoff vs the computerd design.

## What is explicitly NOT done here (deferred / non-goals)

Per `AgentSandbox.md` §3: no permanent desktops/VMs, no Windows, no GPU, no
uncontrolled internet egress, no in-sandbox Kubernetes API, no arbitrary inbound
ports. Interactive PTY / notebooks / port-proxy (the open-terminal extras) are
out of scope for v1, same as in the computerd change's Phase 5.

## Open questions for the implementer (resolve before Phase 1 apply)

1. D1: dedicate+taint sandbox nodes, or shared workers first?
2. Build location: this planning repo (`agent-sandbox-platform/` subdir) or a new
   dedicated repo?
3. Apply target: production cluster now, or a dev overlay/namespace first?
4. S3 backend selection (D7) — confirm at Phase 3.
