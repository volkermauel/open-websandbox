# Security model

open-websandbox runs user-supplied code (shell commands, package installs) inside per-chat
Linux sandboxes. This page documents the threat model, the isolation layers, and the
residual risks accepted for v0.1.0.

## Threat model

**Target users: trusted internal users (Entra OIDC).** The goal is **strong *practical*
sandboxing + prevention of accidental cross-session leakage** — not resistance to a
dedicated, hostile tenant. A **shared host kernel is an accepted residual risk**. If you
must run mutually-hostile tenants, use separate clusters (kernel-level isolation is out of
scope for v0.1.0). See `openspec/changes/archive/adopt-agent-sandbox/design.md` for the decision log.

## Isolation layers (defense in depth)

1. **gVisor (`runsc`)** — each sandbox pod runs under the `gvisor` RuntimeClass: a userspace
   kernel intercepts the guest's syscalls, so the sandbox never sees the host kernel
   directly. Node setup: [`../infra/gvisor/`](../infra/gvisor/).
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

## sudo-apt posture (workbench image)

The [workbench toolchain](toolchain.md) image gives the sandbox user passwordless
sudo for exactly the apt-get verbs (`update, install, remove, purge, upgrade,
full-upgrade, clean, autoremove` — `/etc/sudoers.d/sandbox`, `visudo -c`-checked at
build). Every invocation, allowed or denied, is appended to `/var/log/sudo.log` inside
the sandbox. Nothing else gets sudo; `npm`/`pip` stay user-mode by design.

Why this is acceptable: **apt maintainer scripts run as root, but only inside the
sandbox's own containment** —

- **gVisor (`runsc`)**: the root the scripts get is a userspace-kernel guest root, not the
  host's. runsc runs with `allow-suid` (setuid emulation enabled — gVisor [#5299](https://github.com/google/gvisor/issues/5299);
  without it the setuid `sudo` binary cannot elevate at all): the elevation is emulated
  inside the sentry and never confers host privileges.
- **Default-deny egress**: apt itself can only reach public-internet DNS/HTTP(S); the
  same NetworkPolicy blocks RFC1918, link-local (incl. cloud IMDS), the API server, and
  peer sandboxes — no postinst script can probe or phone the cluster.
- **Ephemeral rootfs**: whatever the scripts write to the rootfs dies with the pod;
  nothing persists into the next session except `/workspace`.
- **No host mounts, no service-account token, caps dropped**: the sandbox has no
  Kubernetes identity and no privileged device to reach for.

This is the reason the **runtime container now sets `readOnlyRootFilesystem: false` and
`allowPrivilegeEscalation: true`** (both were the restricted defaults): `apt-get install`
must write `/usr`, `/etc`, and `/var/lib/dpkg` in the sandbox's own rootfs, and the setuid
`sudo` binary needs to escape the no-new-privileges regime that
`allowPrivilegeEscalation: false` imposes. The runtime container therefore leaves the
*restricted* Pod Security profile on these two axes (the runtime namespace sets no enforce
labels); broker and router stay fully restricted. The container keeps `drop: ["ALL"]`
plus the container-default capability set minus `NET_RAW` (`AUDIT_WRITE`, `CHOWN`,
`DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `MKNOD`, `NET_BIND_SERVICE`, `SETFCAP`,
`SETGID`, `SETPCAP`, `SETUID`, `SYS_CHROOT`): an entirely empty bounding set would strip
the setuid `sudo` binary of every capability at `exec`, and `apt`'s maintainer scripts
(chown/setuid on installed files, service starts) need the working set anyway. Everything the runtime
itself serves still runs as uid 1000; only the whitelisted apt verbs execute as root,
and only within the pod's own filesystem.

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
- **apt maintainer scripts as root (workbench image)** — `sudo apt-get install` runs
  distribution maintainer scripts as root *inside the sandbox* (gVisor + default-deny
  egress + ephemeral rootfs, see [sudo-apt posture](#sudo-apt-posture-workbench-image)).
  A hostile package *chosen by the tenant* could still wreck that tenant's own pod —
  which they can already do from `/execute`. Cross-tenant impact is contained by the
  isolation layers above.

## Reporting a vulnerability

See [`../SECURITY.md`](../SECURITY.md).

## Image signature & SBOM verification

Every `v*.*.*` tag release **cosign-signs all three images (keyless, sigstore via
GitHub OIDC)** and attaches a **per-image SBOM (SPDX-JSON, generated with [syft](https://github.com/anchore/syft))**
to the GitHub Release for that tag. The signature is pushed to the registry as an
OCI ref-tag (`<image>:sha256-<digest>.sig`).

> The published images are `ghcr.io/volkermauel/open-websandbox-{broker,runtime,router}:<tag>`
> (substitute the repo owner if you fork). Note the repo is `open-websandbox` but the
> image names are `open-websandbox-*`.

### Verify the image signature (keyless)

Keyless verification pins the **OIDC issuer** (always GitHub Actions) and the
**workflow identity** (repo + workflow file + tag ref). A forged or re-tagged image
fails verification.

```bash
TAG=v0.1.1                                                            # the tag you pulled
IMAGE=ghcr.io/volkermauel/open-websandbox-broker                         # ...or -runtime / -router

cosign verify --yes \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/volkermauel/open-websandbox/.github/workflows/release.yml@refs/tags/${TAG}" \
  "${IMAGE}:${TAG}"
```

- `--certificate-oidc-issuer` — `https://token.actions.githubusercontent.com` for any GitHub Actions OIDC token.
- `--certificate-identity` — the signing workflow's subject: `https://github.com/<owner>/<repo>/.github/workflows/release.yml@refs/tags/<TAG>`.
- A successful run prints the leaf certificate + Rekor transparency-log entry and exits `0`.

### Download the per-image SBOM

The SPDX-JSON SBOM for each image is attached to the GitHub Release (not a cosign
attestation), so fetch it from the Release:

```bash
# Download all image SBOMs (broker/runtime/router) for a tag:
gh release download "${TAG}" --repo volkermauel/open-websandbox \
  --pattern 'open-websandbox-*.spdx.json'

# List declared packages, e.g.: jq '.packages[].name' open-websandbox-broker-${TAG}.spdx.json | sort -u
```

(Pushing the SBOM as a cosign attestation so `cosign verify-attestation` works
end-to-end is tracked as a follow-up; the image signature is already verifiable above.)
