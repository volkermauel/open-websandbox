> **⚠ SUPERSEDED (2026-08-04)** — not proceeded with. We adopted the Kubernetes
> SIG `agent-sandbox` direction (gVisor, one sandbox per session, Go broker);
> see change **`adopt-agent-sandbox`**. This computerd change (density via one shared
> computerd + per-exec nsjail, shared-kernel isolation) is retained only for its
> research and density analysis (`research/01–05`).

## Why

Open WebUI's "Open Terminal" feature currently runs against the **open-terminal** Docker image,
fronted by **open-terminal-k8s-proxy**, which spawns a **dedicated Kubernetes Pod per user (or
per chat)**. That model gives strong isolation but is expensive: every session incurs k8s pod
scheduling, IP/Service/Secret/PVC provisioning, and seconds of cold-start. We want a far denser,
faster runtime without sacrificing practical isolation — and we want it to run on **plain
infrastructure (MicroK8s + Docker/containerd) with no Cloudflare lock-in**, while still reusing
the genuinely sophisticated parts of Cloudflare's open-source **`computer`** project.

Research (`research/04`) established that the three foundation packages of `computer` —
**`dofs`** (SQLite virtual filesystem + content-addressed sync), **`rpc`/capnweb** (streaming
RPC), and **`computerd`** (in-container FUSE mount + `/bin/sh -c` exec supervisor) — contain
**zero Cloudflare-proprietary code** and run on plain Node 22 + `node:sqlite` + `ws` +
`fuse-native`. Only the host facade and container backend (~3 files) are Cloudflare-coupled,
and the `computer.internal` DNS trick they rely on is **redundant** off-Cloudflare (computerd's
reverse-dial target is a `/connect` body parameter). So we can reuse the hard parts verbatim and
write only a thin host.

## Changes

- **One shared `computerd` sidecar, not a pod per session.** A single privileged worker Pod runs
  two containers: the **gateway** (Node, our code) and **`computerd`** (the published
  `ghcr.io/cloudflare/computer-computerd-linux-x64` image, extended with `nsjail`). Sessions are
  multiplexed onto this one `computerd`. No per-session Pod/Service/Secret/PVC.

- **Gateway owns the authoritative VFS; computerd holds a FUSE mirror.** The gateway opens a
  single **dofs `Database`** backed by a SQLite file on a PVC (single writer, WAL). `computerd`
  FUSE-mounts its mirror and reverse-dials the gateway over localhost WebSocket; the vendored
  dofs sync protocol (push/pull, content-addressed, watermark-cursor incremental) keeps them
  consistent. `/files/*` from Open WebUI operate directly on the authoritative Database;
  `/execute` brackets the call (push → `computerd` exec → pull) so commands see the latest files
  and their writes are captured.

- **Per-session isolation via nsjail, not separate containers.** Each `exec` is wrapped (in the
  gateway, by command construction — **no fork of computerd required**) in an `nsjail`
  invocation that bind-mounts only the session's VFS subtree as `/workspace` (rw), makes the
  rootfs read-only, assigns a per-session uid/gid, optionally unshares the network namespace,
  and applies rlimit/cgroup CPU-memory-time caps. Sessions cannot see each other's files or
  processes at the application level.

- **Reproduce the open-terminal REST/OpenAPI contract (Phase 1 = LLM tool surface).** The gateway
  implements `/execute` + `/execute/{id}/{status,input}` + `/execute/{id}` (DELETE) +
  `/files/{cwd,list,read,write,mkdir,move,delete,replace,grep,glob,display,view,serve,upload}`
  - `/health` + `/api/config` + `/system` + `/info` + `/openapi.json`, with the **same
  `operation_id`s and Pydantic-equivalent shapes** open-terminal exposes, so Open WebUI's native
  integration works unchanged (Bearer `OPEN_TERMINAL_API_KEY` + `X-User-Id`/`X-Session-Id`).
  Interactive PTY terminals (`/api/terminals`), notebooks, and `/proxy/{port}` are deferred.

- **Idle-reaping, caps, and control-plane resilience we build ourselves.** `computer` ships no
  reaping or multi-tenancy. The gateway tracks per-session last-activity, enforces global and
  per-user session caps (evict oldest idle; never evict an in-use session pinned by an active
  exec/stream), and on restart re-adopts the surviving `computerd` + reopens the PVC SQLite so
  sessions resume without data loss.

- **No Cloudflare runtime dependency.** Vendored (MIT, attribution preserved): `dofs`, `rpc`.
  Reused as a published image: `computerd`. The host facade / Durable-Object / Containers-for-
  Workers code is deliberately **not** used.

## Capabilities

### New Capabilities

- `computerd-runtime-host` — a Node gateway that owns the authoritative dofs VFS on a PVC,
  drives one shared `computerd` sidecar (FUSE mirror + exec) over localhost capnweb, and exposes
  the open-terminal Phase-1 REST/OpenAPI surface.
- `session-isolation` — nsjail-based per-session confinement of every `exec` (subtree bind,
  read-only rootfs, per-session uid/gid, optional netns, resource caps) and the documented
  residual-risk model.
- `tenant-routing-auth` — Open WebUI edge: shared Bearer validation, required `X-User-Id`,
  optional `X-Session-Id` with safe fallback, session→VFS-subtree path sanitisation, per-user
  caps.
- `session-lifecycle` — on-demand subtree creation, idle-reaping, global/per-user caps,
  in-use pinning, and control-plane-restart re-adoption.

### Modified Capabilities

*None yet* — greenfield repo (no `openspec/specs/` baseline exists).

## Impact

- **New code (`gateway/`, TypeScript):** ~500–700 LOC — `FileSQLiteStorage` (dofs on PVC),
  localhost WS accept + `/connect` driver, vendored dofs/rpc wiring, exec **push/exec/pull**
  bracket, **nsjail command wrapper**, the open-terminal REST surface + OpenAPI, auth+routing,
  idle-reaping + caps + re-adopt. See `tasks.md`.
- **Vendored packages (MIT):** `dofs` (`packages/dofs/src/**`), `rpc` (`packages/rpc/src/**`).
  Computerd used as-is via its published image + a thin custom image layer installing `nsjail`.
- **Deployment:** a new privileged `Deployment` on MicroK8s (gateway + computerd sidecar) +
  one RWO PVC for the dofs SQLite (+ optional per-user quotas). Single replica for v1; sharding
  by user-hash is the documented HA/scale path.
- **Open WebUI:** zero change — re-point the existing "Open Terminal" connection at the gateway
  URL with the same `OPEN_TERMINAL_API_KEY`; `X-User-Id`/`X-Session-Id` header forwarding stays
  identical to today.
- **Risks (detailed in `design.md`):** nsjail stdio passthrough through computerd's pipe runner
  (Phase-0 spike); dofs single-writer correctness under the gateway-owned-authority model;
  `/dev/fuse` availability in the privileged MicroK8s pod; residual shared-kernel isolation
  weakness (accepted by threat model); Phase-1 deferral of interactive PTY / port-proxy.
