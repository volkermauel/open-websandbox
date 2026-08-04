# Tasks — adopt-agent-sandbox

Phased rollout of the Kubernetes SIG `agent-sandbox` platform onto the on-prem
MicroK8s cluster. Phase numbering aligns with `AgentSandbox.md` §22. The detailed
architecture, invariants, and manifests live in `AgentSandbox.md`; this file
tracks scoping onto our cluster. Cite `design.md` decision IDs (D#).

## Phase 0 — Runtime isolation  ✅ DONE (2026-08-04)

- [x] **0.1** gVisor `runsc` (`release-20260727.0`) + `containerd-shim-runsc-v1`
      installed on `gvisor-worker-1/w2/w3` via the snap-MicroK8s template.
- [x] **0.2** `/etc/runsc/config.toml` → `platform = "systrap"` (no nested virt).
- [x] **0.3** `containerd-template.toml` runsc handler (rendered `containerd.toml`
      carries it after each restart). MicroK8s does not auto-restart on edit.
- [x] **0.4** Online-safe containerd restart on all three workers — zero pod
      disruption (verified per node). CNPG primaries (on `w1`) failed over to
      `w2`/`w3` first via `kubectl cnpg promote` before the `w1` restart.
- [x] **0.5** `RuntimeClass` `gvisor` (handler `runsc`) cluster-wide; probe pods
      confirm `4.19.0-gvisor` / "Starting gVisor..." on each worker.
- [x] **0.6** Reproducible playbook: `infra/gvisor/` (install/activate scripts,
      RuntimeClass + verify manifests, README with the CNPG caveat).

## Phase 1 — Controller + router

- [x] **1.1** Decide: dedicate+taint sandbox worker nodes (§6.2) vs run on shared
      workers initially (D1). **DECISION (2026-08-04): shared workers first**
      (RuntimeClass Variant A). gVisor is active on w1/w2/w3; dedicate+taint
      deferred to Phase 6 if the threat model needs node-level separation.
- [x] **1.2** Pin `agent-sandbox` `v0.5.3`; vendored the install manifest to
      `upstream/sandbox-with-extensions-v0.5.3.yaml`; `SHA256SUMS` recorded
      (b7c047f2…). Controller image pinned `:v0.5.3` (digest-pin deferred to Phase 6).
- [x] **1.3** Namespaces: `agent-sandbox-system` (created by the manifest) +
      `agent-sandbox-runtime` (created, `restricted` Pod Security enforce/audit/warn).
      `agent-sandbox-observability` optional, deferred.
- [x] **1.4** Controller + core/extension CRDs applied; all 4 CRDs Established=True
      (`agents.x-k8s.io` Sandbox; `extensions.agents.x-k8s.io` SandboxClaim/
      SandboxTemplate/SandboxWarmPool). ClusterRole RBAC reviewed (no wildcards,
      no secrets/nodes access).
- [x] **1.5** Go router deployed: 2 replicas, ClusterIP `sandbox-router-svc`, topology
      spread, non-root/drop-caps/seccomp, probes; `/healthz`→200. **Image gap:** upstream
      publishes no versioned `sandbox-router-go` (`:latest` also 404s), so self-built from
      source (`sandbox-router-go:dev`, `docker build -f sandbox-router/Dockerfile` with
      repo-root context) and loaded into all 3 workers via `microk8s ctr images import`
      (k8s.io ns), `imagePullPolicy: Never`. Phase 6: internal registry + digest pin.
- [x] **1.6** NetworkPolicy applied: router NP (ingress only from agent-sandbox-system
      on 8080; egress to APIserver 10.96.0.1:443 + DNS + sandbox:8888) + sandbox-runtime
      default-deny (ingress only from router on 8888, DNS egress). Router informers verified
      healthy post-NP (APIserver egress correct).
- [x] **1.7** ResourceQuota + LimitRange applied in agent-sandbox-runtime (§15.1).
- [x] **1.8** Verify controller healthy; smoke DONE — a `Sandbox` CR
      (`smoke-gvisor`) was reconciled into a gVisor pod on w2 (`uname
      4.19.0-gvisor`, `dmesg` "Starting gVisor...", `runtimeClassName=gvisor`),
      then cleaned up. Router-proxy exec pending 1.5.

## Phase 2 — Runtime image + warm pool

- [x] **2.1** Built `code-standard:v1` (`python:3.12-slim` + `build-essential`/
      `python3-dev` + `nodejs`/`npm` + git/curl/jq; non-root `1000:1000`;
      `/workspace`,`/home/sandbox`,`/tmp`; no ssh/docker/k8s creds). Ships a curated
      "agent-common" library set (PyYAML, numpy, pandas, openpyxl, requests, bs4,
      lxml, …) for warm-ready first use + dynamic pip/npm (incl. native builds).
      SBOM/scan + digest-pin deferred to Phase 6.
- [x] **2.2** Runtime server (`runtime/server.py`, FastAPI/uvicorn): `POST /execute`
      (OWUI shell-string contract), `/upload` `/download` `/list` `/exists`, per-call
      command timeout, 1 MiB output cap, whole-process-tree kill (`start_new_session`
      + `os.killpg` on timeout). Security boundary is gVisor+uid1000+RO-root+NP, not
      argument parsing. Async job cancellation deferred.
- [ ] **2.3** Unit tests outside Kubernetes (pending).
- [x] **2.4** Deployed `SandboxTemplate` `code-standard-v1` (runtimeClassName
      `gvisor`, `automountServiceAccountToken: false`, drop ALL caps, runAs 1000,
      read-only root, emptyDir sizeLimits, fsGroup 1000) + `SandboxWarmPool`
      (`replicas: 2`). Image loaded into w1/w2 via `microk8s ctr images import`
      (`imagePullPolicy: Never`); **w3 image sync pending** (1 GB transfer timed out —
      pods currently schedule on w1/w2). Persistent profile (`volumeClaimTemplates`
      PVC at `/workspace`) spec'd; deployed with the broker (Phase 3).
- [x] **2.5** Warm capacity verified: 2 pods `Ready 1/1` on gVisor
      (`4.19.0-gvisor`); `GET /` + `POST /execute` work (uid 1000, cwd `/workspace`);
      curated libs import (`pandas 3.0.5`); dynamic `pip install arrow` succeeds
      (open-443 egress NP correct). `SandboxClaim` flow + cold-start budget pending
      the broker (Phase 3).

## Phase 3 — Broker

- [ ] **3.1** Go broker: `/v1` API (sessions, exec, files, delete); auth middleware
      trusting only the OIDC reverse proxy identity headers.
- [ ] **3.2** Profile/quota policy (allowed_groups, per-user/per-tenant caps,
      max_lifetime, idle_timeout).
- [ ] **3.3** `SandboxClaim` create/watch/delete; signed opaque session tokens;
      header strip + router-auth inject on proxy.
- [ ] **3.4** Idle (≥1/min) + absolute-maximum expiry reconciler; quarantine
      ownerless/overdue claims.
- [ ] **3.5** Stateless restart recovery: rebuild sessions from broker-owned
      claims (`sandbox.open-websandbox.local/created-by=broker`).
- [ ] **3.6** Metrics, structured logs, audit events (no prompts/commands/secrets
      in logs). Optional MCP adapter (`sandbox_*` tools).
- [ ] **3.7** OWUI adapter mapping the broker API to Open WebUI's tool surface.

## Phase 4 — Network + admission controls

- [ ] **4.1** Runtime default-deny `NetworkPolicy`; allow router ingress, DNS,
      S3, policy-controlled egress proxy.
- [ ] **4.2** `ValidatingAdmissionPolicy` enforcing §4 invariants (gVisor-only,
      node selector/toleration, no host*, non-root, drop caps, limits, pinned
      images, no mutable tags in prod).
- [ ] **4.3** Confirm broker/router not directly reachable from unauthorized
      namespaces; cross-tenant access returns 403 pre-router.

## Phase 5 — End-to-end tests (§22 Phase 6 matrix)

- [ ] **5.1** Warm + cold session create; exec/upload/download/delete; idle +
      absolute expiry; broker/router restart recovery; node drain; quota;
      cross-user/tenant access denial; header spoofing; network + Kubernetes-API
      + host-fs isolation; resource exhaustion; fork-bomb PIDs; output flooding;
      symlink traversal; cleanup + warm-pool replenishment.

## Phase 6 — Production overlay

- [ ] **6.1** Pin every image by digest; remove mutable tags/placeholders.
- [ ] **6.2** GitOps (argocd) deployment; overlays for the cluster.
- [ ] **6.3** Decide + implement dedicated/tainted sandbox nodes if not done in 1.1.
- [ ] **6.4** Dashboards, alerts (§19.4), runbooks (§25), upgrade + rollback
      procedures; security review against §4 invariants.
