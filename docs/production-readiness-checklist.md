# open-websandbox v1.0 Production-Readiness Checklist

`[M]` = table-stakes for a credible v1.0 · `[A]` = advanced / beyond-v1.
Diff this against actual state. Refs: agent-sandbox (kubernetes-sigs), Eclipse Che/Devfile, Coder, Gitpod, code-server, vcluster, CNCF release norms.

## 1. Release & Packaging

- [M] **Signed OCI images + Helm chart (cosign keyless), SBOM, SLSA provenance.** *Why:* tenants run your binaries with cluster perms; Helm charts are the least-verified link in most K8s supply chains (no enforced provenance by default).
- [M] **Chart lints clean (`kubeconform`/`kyverno`/`kube-score`), values schema, no `tpl` injection, scoped RBAC.** *Why:* a chart bundling a `cluster-admin` ClusterRoleBinding grants adopters' clusters to unaudited code.
- [M] **SemVer tags + CHANGELOG with BREAKING / upgrade-notes gate.** *Why:* operators must know pre-upgrade whether sessions or per-user PVCs get re-cloned.
- [A] **Reproducible builds + multi-arch (amd64/arm64).**

## 2. Security & Multi-tenant Isolation

- [M] **gVisor `runtimeClassName` enforced by ValidatingAdmissionPolicy (agent-sandbox "Secure by Default" VAP).** *Why:* one tenant pod missing gVisor = a container-escape path to the host for every tenant.
- [M] **PSS `restricted` enforced on all control-plane + tenant namespaces.** *Why:* baseline posture for running untrusted code (Kubernetes Pod Security Standards).
- [M] **Per-user ServiceAccount + least-priv RBAC + deny-by-default per-tenant NetworkPolicy + OIDC auth.** *Why:* Eclipse Che is "multi-tenant by default" via OIDC+NetworkPolicy; without it the sandbox is an open shell and cross-tenant lateral movement is the #1 multitenant risk (Coder/Loft).
- [M] **WS terminal proxy: per-user authz, origin/CSRF hardening, rate limits.** *Why:* code-server CVEs (session-cookie theft via crafted proxy URL) prove a misconfigured terminal proxy = host RCE.
- [M] **Per-tenant CPU/mem/PVC quotas; KMS-encrypted etcd + volumes.** *Why:* noisy-neighbor + data-at-rest protection for a shared control plane.
- [A] **Per-tenant node pools/taints, seccomp-Strict, tenant egress allowlist.**

## 3. Observability

- [M] **Prometheus metrics incl. session count, warm-pool hit-rate, PVC usage, gVisor faults.** *Why:* warm-pool hit-rate + sandbox churn are the SLOs unique to this runtime.
- [M] **Structured JSON logs with tenant-id correlation, no cross-tenant PII leakage.** *Why:* multi-tenant means one tenant's logs must never expose another's.
- [M] **OTel traces across broker → warm-pool → pod → WS proxy.** *Why:* session routing spans components; latency needs end-to-end traces.
- [A] **Shipped Grafana dashboards + alert rules + runbooks.**

## 4. Scale & High Availability

- [M] **Broker / control plane HA (≥3 replicas, leader election, PDB).** *Why:* the broker is the session-routing brain; its failure kills every active terminal.
- [M] **Warm pool autoscales on queue depth / hit-rate with headroom.** *Why:* cold-start latency dominates UX for a sandbox.
- [M] **PVC storageclass `WaitForFirstConsumer` + topology spread; WS proxy horizontally scalable.** *Why:* per-user PVCs must follow pod scheduling or sessions fail to attach.
- [A] **Multi-AZ broker, node-cordon session drain, load-tested to 10k+ concurrent WS.**

## 5. Operations / Day-2 (upgrade / rollback / backup / DR)

- [M] **Documented, tested upgrade + rollback that preserves sessions and never drops PVC data (snapshot-before-upgrade, vcluster-style).** *Why:* per-user PVCs are irreplaceable tenant data.
- [M] **Backup/restore for control-plane state + CSI PVC snapshots (Velero), with a tested restore drill.** *Why:* DR for tenant home dirs.
- [M] **Graceful warm-pod / session eviction on scale-down + reclaim logic.** *Why:* killing a warm pod mid-session corrupts user state.
- [A] **CRD status conditions + migration harness + chaos tests.**

## 6. Docs & Support

- [M] **Install/upgrade/backup runbooks + published threat model (cf. agent-sandbox `threat_model.md`).** *Why:* a sandbox carries trust requirements; operators must see the model before granting tenant code execution.
- [M] **Versioned docs site + supported-K8s matrix + deprecation policy + `SECURITY.md` disclosure process.** *Why:* CNCF project norm; adopters need upgrade-safety + vuln-reporting guarantees.
- [M] **`ADOPTERS.md` + reproducible issue/bug-report template.**
- [A] **Community: SIG calls, public roadmap, issue-triage SLA.**
