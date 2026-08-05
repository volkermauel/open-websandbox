# Architecture

**open-sandbox** is a Kubernetes runtime that backs Open WebUI's "Open Terminal"
feature. Each chat gets an isolated Linux sandbox running under **gVisor
(`runsc`)**; an agent (or a human in the terminal UI) can run shell commands,
edit files, and install packages as if on a throwaway VM — without that VM's
blast radius.

This document describes the component flow, the per-chat sandbox lifecycle, the
two workspace modes, and the isolation layers. The decision log and rationale
live in [`openspec/changes/adopt-agent-sandbox/design.md`](../openspec/changes/adopt-agent-sandbox/design.md)
and the authoritative spec [`AgentSandbox.md`](../AgentSandbox.md).

## Components

| Component | Language | Where it runs | Role |
|-----------|----------|---------------|------|
| **broker** | Python (FastAPI) | `agent-sandbox-system` | Front door. Authenticates Open WebUI, resolves/creates the sandbox for a user+session, reverse-proxies the request to the runtime. Owns the idle reaper. ([`agent-sandbox-platform/broker/main.py`](../agent-sandbox-platform/broker/main.py)) |
| **sandbox-router** | Go | `agent-sandbox-system` | Reverse proxy that dials the live sandbox Pod IP. Keeps a Pod-IP cache (watches sandbox-owned pods cluster-wide) for a fast path; falls back to cluster DNS. ([`agent-sandbox-platform/deploy/base/router/`](../agent-sandbox-platform/deploy/base/router/)) |
| **runtime** | Python (FastAPI) | each sandbox pod (`agent-sandbox-runtime`) | The in-sandbox server: `POST /execute`, `GET/POST /files/*`, `GET /ports`, `POST /api/terminals` + `WS /api/terminals/{id}` (PTY). ([`agent-sandbox-platform/runtime/server.py`](../agent-sandbox-platform/runtime/server.py)) |
| **agent-sandbox controller** | Go (upstream) | `agent-sandbox-system` | Reconciles the CRDs (`Sandbox`, `SandboxTemplate`, `SandboxWarmPool`, `SandboxClaim`) into pods and binds claims to warm-pool sandboxes. Pinned at **v0.5.3**. ([`agent-sandbox-platform/upstream/`](../agent-sandbox-platform/upstream/)) |

The controller is an **external** dependency — `kubernetes-sigs/agent-sandbox`.
We vendor its install manifest (SHA256-recorded) and never modify it; everything
else in this repo is ours.

## Request flow

```
                       agent-sandbox-system                          agent-sandbox-runtime
  ┌──────────┐        ┌────────────────┐   HTTP (X-Sandbox-Pod-IP)   ┌─────────────────────┐
  │ Open Web │───────▶│     broker     │───────────► sandbox-router ─▶│  sandbox pod        │
  │   UI     │  ①auth │ resolve sandbox│ ③ proxy   (Pod-IP fast path) │  gVisor + runtime   │
  │          │  ②hdrs │ get/create claim│           ④ dial 8888       │  :8888 /execute ... │
  └──────────┘        └────────┬───────┘                             └─────────────────────┘
         │                      │                                              ▲
         │   WS terminal ────────────────────────────────────────────────────┘
         └──────────────────▶ broker resolves Pod IP, then dials ⑤ ws://<pod-ip>:8888
                              DIRECTLY (bypasses router — its WS upgrade handshake
                              is slow/fragile under load)

  Controller (system ns) reconciles: SandboxClaim ──binds──▶ warm-pool Sandbox ──owns──▶ Pod
```

1. **Auth + headers.** Open WebUI calls the broker with a shared bearer
   (`Authorization`) plus `X-User-Id` and `X-Session-Id` per session. The broker
   checks the bearer in constant time (`hmac.compare_digest`).
2. **Resolve sandbox.** The broker get-or-creates the right CRD for the
   user/session/profile (see [Lifecycle](#lifecycle) and
   [Workspace modes](#workspace-modes-ephemeral-vs-persistent)), waits for the
   sandbox `Ready` condition, and reads the live Pod IP from the resource
   status.
3. **Proxy HTTP** (`/execute`, `/files/*`, …). The broker forwards to the
   `sandbox-router` Service, injecting `X-Sandbox-Pod-Id`,
   `X-Sandbox-Namespace`, and `X-Sandbox-Pod-IP`. The router uses Pod-IP as
   priority-1 resolution and dials the pod directly on `:8888`.
4. **Terminal WebSocket** (`/api/terminals/{session_id}`). The broker resolves
   the Pod IP itself and opens `ws://<pod-ip>:8888/...` **directly** — the
   NetworkPolicy permits ingress from `agent-sandbox-system` on 8888, and
   bypassing the router avoids its WebSocket-upgrade timeout. The first WS
   frame is `{"type":"auth","token":<shared-secret>}`.

> The runtime never trusts client-supplied routing identifiers. The broker
> derives every sandbox name deterministically from `X-User-Id` /
> `X-Session-Id` server-side; headers a user might try to forge are ignored.

## Lifecycle

A sandbox is **claimed per chat**, not per user-account. Three states move
through one loop:

```
   WarmPool ──claim──▶ Running ──idle>PARK──▶ Suspended ──idle>REAP──▶ (deleted)
      ▲                   │  (persistent only)   │                          │
      │                   │  resumed on next     │                          │ PVC retained
      │ ◀──replenish────  └────── request ───────┘                          │ while parked;
      │             (ephemeral: idle>IDLE_TTL  reap ⇒ sandbox returns here) ▼
      │                                                                claim+PVC deleted
      └──────────────────────────────────────────────────────────────────┘
```

The broker runs an idle **reaper** every 60 s (`_reaper_loop` in
[`broker/main.py`](../agent-sandbox-platform/broker/main.py)) that lists all
resources labelled `app.kubernetes.io/managed-by=owui-broker` and acts by
`broker-last-used` annotation age:

| Profile / resource | On idle > `PARK_IDLE_SECONDS` (120 s) | On idle > `REAP_SECONDS` (7 d) |
|--------------------|---------------------------------------|--------------------------------|
| **ephemeral** `SandboxClaim` (`owui-<hash>`) | — | claim **deleted**; its sandbox is released back to the warm pool, which replenishes a fresh one. (Also bound by `IDLE_TTL_SECONDS`, default 120 s.) |
| **persistent** `SandboxClaim` (`owui-p-<hash>`) or per-chat `Sandbox` (`owui-c-<hash>`) | sandbox `operatingMode` patched to **`Suspended`** — pod deleted, node freed, **PVC retained**. | claim **deleted**, PVC freed. |

Resuming a parked persistent sandbox (next request) sets `operatingMode: Running`
and waits for a fresh pod + Pod IP (cold-start ~1–6 s). Warm-pool ephemeral
sandboxes are always `Running`, so a fresh ephemeral claim binds instantly.

> Density model: there is **no permanent pod per user**. Pods exist only while a
> chat is active (ephemeral) or recently active (persistent, until parked). The
> warm pool hides cold-start; parking frees nodes. This is the core trade vs. a
> one-daemon-per-host design.

## Workspace modes: ephemeral vs. persistent

`/workspace` is the agent's working directory. What backs it is chosen **at
deploy time** by `BROKER_DEFAULT_PROFILE` (Open WebUI can't send custom request
headers, so the profile can't vary per call); an explicit `X-Persistence`
header/query still overrides it for admin/testing. See
[`deploy.md`](deploy.md#broker-configuration) for the env vars.

### Ephemeral (default-warm-pool)

- `/workspace` is an **emptyDir** (`sizeLimit: 4Gi`) defined in the
  `code-standard-v1` SandboxTemplate.
- One claim per **session**: `owui-<sha256(user|session)[:12]>`.
- Files are destroyed when the claim is reaped (idle or session end). The warm
  pool then builds a clean replacement.

### Persistent (deploy default: `persistent`)

`/workspace` survives pod/image rollouts on a **`cephfs` RWX** volume.
Two backends, selected by `BROKER_PERSISTENT_MODE`:

| Mode | CRD created | Volume | Per-chat isolation |
|------|-------------|--------|--------------------|
| **`per-user-pvc`** (default) | `SandboxClaim` `owui-p-<sha256(user)>` with a `volumeClaimTemplates.workspace` PVC | one RWX PVC **per user** | broker injects `X-Workspace-Subdir` per chat; runtime confines all file ops under `/workspace/<subdir>` |
| **`shared-subpath`** | per-chat `Sandbox` `owui-c-<sha256(user/session)>` | one shared `workspace-shared` PVC; each sandbox mounts `users/<id>/` via `subPath` | hard mount isolation — each chat sees only its slice |

Either way two concurrent chats of the **same** user cannot read each other's
files by default, while still sharing the user's installed packages. The runtime
validates the subdir name and rejects any path that escapes the effective base
(`_safe_path` / `_request_base` in `runtime/server.py`).

## Isolation layers

Every sandbox pod is locked down at four independent layers — a breach of one
does not grant the others:

1. **gVisor userspace kernel.** `runtimeClassName: gvisor` (runsc, systrap).
   Syscalls are intercepted and filtered in userspace; the guest never touches
   the host kernel directly. Node setup: [`infra/gvisor/`](../infra/gvisor/).
2. **Non-root, least privilege.** `runAsUser/runAsGroup/fsGroup: 1000`,
   `runAsNonRoot: true`, **all Linux capabilities dropped**, seccomp
   `RuntimeDefault`. (See the `securityContext` in
   [`sandboxtemplate-code-standard.yaml`](../agent-sandbox-platform/deploy/base/sandboxtemplate-code-standard.yaml).)
3. **No Kubernetes identity.** `automountServiceAccountToken: false`; the
   `KUBERNETES_SERVICE_*` env vars are explicitly unset so the in-sandbox
   service CIDR isn't even discoverable. Users never receive kubeconfig or
   tokens; only the broker (narrowly scoped `Role` in
   [`broker.yaml`](../agent-sandbox-platform/deploy/base/broker.yaml)) creates
   claims.
4. **Default-deny network.** The runtime namespace
   [`NetworkPolicy`](../agent-sandbox-platform/deploy/base/networkpolicy-runtime.yaml)
   allows only:
   - **ingress** from `agent-sandbox-system` on TCP/8888 (router + broker), and
   - **egress** to public DNS resolvers (UDP/TCP 53 to 8.8.8.8 / 1.1.1.1 — the
     template also sets `dnsPolicy: None` so no cluster DNS IP or namespace
     search domain leaks) and to **HTTPS+HTTP (443/80) on the public internet
     only**, with **all RFC1918 + link-local CIDRs blocked** (anti-lateral
     movement: a sandbox cannot reach the API server, other pods, or internal
     services).

   Pip/npm/git installs work over that public HTTPS egress; anything inside the
   cluster is unreachable. (A later phase replaces open-443 with an
   allowlist egress proxy — tracked in the spec.)

Per-sandbox resource caps complement the above: per-command timeout
(`DEFAULT_TIMEOUT` 120 s, capped at `MAX_TIMEOUT` 600 s), stdout/stderr truncated
at `MAX_OUTPUT_BYTES` (1 MiB), process count capped via `MAX_PROCS`
(`RLIMIT_NPROC` 256, enforced under gVisor), and `/tmp` on a **tmpfs with a hard
ENOSPC cap** (`emptyDir.medium: Memory`, 2 Gi) so a sandbox can't exhaust node
disk.

## What is explicitly out of scope

Per the spec non-goals: no permanent desktops/VMs, no Windows, no GPU, no
uncontrolled internet egress, no in-sandbox Kubernetes API, no arbitrary inbound
ports. Interactive notebooks/desktops and port-proxying are not in v1.
