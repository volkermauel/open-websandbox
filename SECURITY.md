# Security Policy

## Reporting a vulnerability

open-websandbox is a sandboxing platform. If you find a security issue **in open-websandbox
itself** (the broker, runtime, router, Helm chart, or its deployment configuration), please
report it privately — **do not open a public GitHub issue**.

- **Preferred:** GitHub's private vulnerability reporting (Security tab →
  "Report a vulnerability").
- **Otherwise:** email the maintainers (see the repository profile).

Please include: a description, reproduction steps, the affected component and version, and
the impact. We aim to acknowledge within **3 business days** and to coordinate a fix and
disclosure timeline with you.

## Scope

This policy covers the open-websandbox **control plane** — the broker, router, and runtime,
the Helm chart, and the documented deployment (NetworkPolicy, RBAC, the gVisor runtimeClass,
PodSecurity). It does **not** cover:

- code you **run inside** a sandbox — containing that is the sandbox's job (and the user's
  responsibility);
- the upstream **kubernetes-sigs/agent-sandbox** controller, or **gVisor/runsc** itself —
  report those upstream;
- the host Kubernetes cluster or its nodes.

## Threat model (summary)

open-websandbox targets **trusted internal users**. It provides strong *practical* sandboxing —
gVisor (runsc), uid isolation, no service-account token, restricted NetworkPolicy egress,
per-chat sandbox separation — and prevents accidental cross-session leakage. A **shared
kernel is an accepted residual risk**; resisting a dedicated, hostile tenant is explicitly
**out of scope** for v0.1.0. See `openspec/changes/adopt-agent-sandbox/design.md` for the
full model and `docs/security.md` for the layer-by-layer breakdown.

## License

open-websandbox is licensed under the **GNU Affero General Public License v3.0 only**
(`AGPL-3.0-only`); see [`LICENSE`](LICENSE). This policy covers the open-websandbox
control plane only — code run *inside* a sandbox is the user's responsibility.
