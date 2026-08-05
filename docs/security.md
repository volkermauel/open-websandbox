# Security model

open-sandbox runs user-supplied code (shell commands, package installs) inside per-chat
Linux sandboxes. This page documents the threat model, the isolation layers, and the
residual risks accepted for v0.1.0.

## Threat model

**Target users: trusted internal users (Entra OIDC).** The goal is **strong *practical*
sandboxing + prevention of accidental cross-session leakage** — not resistance to a
dedicated, hostile tenant. A **shared host kernel is an accepted residual risk**. If you
must run mutually-hostile tenants, use separate clusters (kernel-level isolation is out of
scope for v0.1.0). See `openspec/changes/adopt-agent-sandbox/design.md` for the decision log.

## Isolation layers (defense in depth)

1. **gVisor (`runsc`)** — each sandbox pod runs under the `gvisor` RuntimeClass: a userspace
   kernel intercepts the guest's syscalls, so the sandbox never sees the host kernel
   directly. Node setup: [`../../infra/gvisor/`](../../infra/gvisor/).
2. **Per-chat sandbox** — one sandbox per chat, keyed `sha256(user_id/session_id)[:12]`.
   Distinct chats get distinct pods, filesystems, and process trees. Nothing is shared
   across chats; only `/workspace` persists (per-user, on a RWX PVC).
3. **uid 1000, non-root, no caps** — sandbox containers run as uid/gid 1000
   (`runAsNonRoot`, `runAsUser`/`runAsGroup`), `seccompProfile: RuntimeDefault`, with
   Linux capabilities dropped.
4. **No service-account token** — `automountServiceAccountToken: false`; the sandbox cannot
   authenticate to the Kubernetes API server.
5. **Default-deny NetworkPolicy egress** — a sandbox may reach ONLY:
   - **DNS** (UDP/TCP 53) to the configured public resolvers (`8.8.8.8`, `1.1.1.1`);
   - **HTTPS (443)** and **HTTP (80)** to the public internet, **excluding all RFC1918 +
     link-local CIDRs** — so it cannot reach the API server, pod/service CIDRs, or node hosts.
   Everything else (the cluster DNS IP, internal services, the broker, other sandboxes) is
   blocked. Ingress is denied except from the router/broker namespace on the runtime port.
6. **Resource caps** — per-uid PID cap (`MAX_PROCS=256`, via `RLIMIT_NPROC`/`prlimit`),
   a tmpfs `/tmp` hard cap (2 GiB via `medium: Memory`), and `emptyDir` `sizeLimit`s on the
   ephemeral volumes. A namespace `ResourceQuota`/`LimitRange` bounds pods, PVCs, and storage.

## What persists (and what deliberately doesn't)

- **Persists** — `/workspace` (on a RWX PVC): work files. Deliberately **not** on `PATH` and
  **not** auto-executed.
- **Ephemeral** (rebuilt per pod) — `/home` (no `.bashrc` autoload ⇒ no cross-session code
  execution), `/packages` (not at the front of `PATH` ⇒ no planted-binary shadowing), `/tmp`
  (tmpfs, wiped), the rootfs.

## Residual risks (v0.1.0)

- **Shared host kernel** — gVisor is a userspace kernel, but the host kernel is still shared
  across sandbox pods on a node. Host kernel 0-days are not mitigated here.
- **procfs visibility** — gVisor cannot fully mask `/proc` (upstream limitation); the
  sandbox's own `/proc/1/environ` is readable by the same uid. Mitigated by blanking the
  `KUBERNETES_*` environment variables and never injecting secrets into sandbox env.
- **Open DNS/HTTP(S) egress** — a sandbox can exfiltrate over 443/80 (encrypted) or via DNS.
  A future domain-allowlisting proxy (Phase 4) tightens this; for now it's an accepted
  channel (HTTPS is already a larger exfil surface).
- **NetworkPolicy is L3/L4 only** — no L7 (content) inspection.

## Reporting a vulnerability

See [`../SECURITY.md`](../SECURITY.md).
