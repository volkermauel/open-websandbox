# Design — use-computerd-as-runtime

> Companion to `proposal.md`. Read `research/01..04` for evidence; this document
> records the **decisions** and the **concrete design**.

## 1. Context & goal restated

Back Open WebUI's "Open Terminal" integration with a runtime derived from Cloudflare's
open-source **`computer`**, running on **plain MicroK8s + Docker/containerd** with **no
Cloudflare-proprietary dependencies**. Reuse `computer`'s hard parts (`dofs`, `rpc`, `computerd`)
verbatim; provide **full per-user isolation** and **per-chat isolation** at **maximum density**
(one shared `computerd`, no pod/container per session). Threat model: trusted internal (Entra
OIDC) users; accepted residual risk = shared kernel.

## 2. Architectural decisions (D1–D14)

- **D1 — One shared `computerd`, sessions multiplexed (not pod/container-per-session).**
  Rationale: user directive for max density. Cost: isolation moves from "separate container"
  to "nsjail jail per exec" (weaker; accepted). `computerd` is single-workspace by design
  (1 FUSE mount, 1 dofs Database) — see D4.

- **D2 — Reuse `dofs` + `rpc`/capnweb + `computerd` verbatim; do NOT use the host facade.**
  `research/04` proves these three are Cloudflare-free (no `cloudflare:` imports, no
  `ctx.storage`/`ctx.container`). `packages/computer`'s `Workspace`/`CloudflareContainerBackend`
  are skipped. Vendoring scope = `packages/dofs/src/**` + `packages/rpc/src/**` (MIT, attribution
  preserved in `NOTICE`). `computerd` consumed as its published image.

- **D3 — Gateway owns the authoritative VFS; `computerd` holds a synced FUSE mirror.**
  dofs is **single-writer** (`research/04` §2, "Single-writer, 1:1 DO:container"). The gateway is
  that single writer: it opens the dofs `Database` against a SQLite file on the PVC. `computerd`
  FUSE-mounts its in-memory mirror and reverse-dials the gateway; the dofs **sync protocol**
  (content-addressed, watermark-cursor, incremental) keeps them consistent over localhost. This
  is the DO↔container model from `computer`, with the gateway playing the DO.

- **D4 — One dofs Database, sessions = VFS subtrees (`/sess/<u>/<c>/`).**
  `computerd` supports exactly one FUSE mount → one dofs Database. All sessions therefore live as
  subtrees of one shared SQLite VFS. Per-session isolation is **not** storage-level (the jail in
  D7 provides confinement); per-session cleanup = delete the subtree; per-session storage cap is
  a soft quota enforced by the gateway (monitor subtree size; reject writes over quota). Scale
  path = shard sessions across multiple worker pods by user-hash (D14), each with its own
  computerd + SQLite.

- **D5 — Exec is bracketed: push → exec → pull.**
  Because the gateway is authoritative, a command run inside `computerd` must (a) see the latest
  files ⇒ **push** gateway→container delta before exec; (b) have its writes captured ⇒ **pull**
  container→gateway delta after exec. Implemented as a ~15-LOC helper around the vendored
  `pullOnce`/`pushOnce`/`tick` sync driver (`research/04` §7 Style A). We do **not** vendor the
  `Workspace` facade (Style B) — Style A is less code, less surface, and the bracketing is
  explicit and auditable.

- **D6 — No fork of `computerd`; isolation applied by command construction.**
  Instead of patching `computerd`'s runner, the gateway wraps each command in an `nsjail`
  invocation and passes that as the exec string: `computerd` runs `/bin/sh -c "nsjail … -- /bin/sh
  -c '<user-cmd>'"` unmodified. Keeps `computerd` pristine (honours `research/04` "REUSABLE-AS-IS")
  and isolates all sandbox policy in our code.

- **D7 — nsjail (not bubblewrap) for per-exec confinement.** nsjail gives namespaces **plus**
  cgroup CPU/mem/time caps **plus** seccomp-bpf in one tool; purpose-built for sandboxing
  untrusted commands. bubblewrap lacks cgroups. Per-exec jail config:
  - `--cwd /workspace` and bind the **session subtree** at `/workspace` (rw) — `--bind
    <mirror-root>/sess/<u>/<c>:/workspace`.
  - read-only rootfs: `--ro-bind / /` (+ writable `/tmp` as a per-exec tmpfs
    `--mount tmpfs:/tmp`).
  - per-session identity: `--clone_newuser --uid <uid(u,c)> --gid <gid(u,c)>` (deterministic
    uid/gid from a hash of (user, chat), mapped into the userns; the subtree is chowned to match
    so the jailed uid can write).
  - **network:** Phase 1 = **shared netns** (no `--clone_newnet`) so commands have egress by
    default. Per-user/per-chat egress policy (D13) = Phase 2, implemented by giving the jail its
    own netns + an egress allowlist/proxy.
  - resources: `--rlimit_f 1024 --time_limit <EXEC_TIMEOUT>` + cgroup `--cgroup_mem_max` /
    `--cgroup_cpu_ms_per_ms` (per-session caps from config).
  - seccomp profile (deny `keyctl`, `kexec`, `ptrace`, `unshare`, …) + dropped capabilities.
  - hostname = session id; `--quiet --really_quiet`.

- **D8 — Deployment topology: one privileged worker Pod = gateway + computerd sidecar.**
  - Two containers in one Pod (shared volumes, localhost networking):
    - **gateway** (Node 22) — the open-terminal REST edge + authoritative dofs + sync + nsjail
      wrapper + lifecycle.
    - **computerd** — `ghcr.io/cloudflare/computer-computerd-linux-x64:0.1.0-alpha.1` + a thin
      layer installing `nsjail`. Env: `FUSE_MOUNT=auto`, mount `/dev/fuse` (privileged
      `securityContext`), `UPSTREAM_URL`/`/connect` target = `http://localhost:<port>`.
  - Shared **RWO PVC** mounted at `/data` holding the dofs SQLite (`/data/vfs.db`, WAL files) and
    the computerd FUSE mirror working set. (The FUSE mountpoint itself lives in computerd's mount
    namespace; the gateway never touches it directly — it talks to computerd over WS only.)
  - `securityContext.privileged: true` (needed for `/dev/fuse` FUSE mount + nsjail namespace/cgroup
    setup). Accepted trade-off for the MicroK8s substrate choice.
  - Single replica for v1 (single dofs SQLite writer). HA/scale = D14.

- **D9 — Reproduce the open-terminal Phase-1 REST/OpenAPI contract verbatim.**
  `research/02` documents the exact surface. The gateway implements (TS, Pydantic-equivalent
  Zod schemas, identical `operation_id`s):

  | open-terminal route | gateway implementation |
  |---|---|
  | `POST /execute` (`run_command`) | allocate id; spawn `exec` via D5 bracket + D6 nsjail wrap; stream stdout/stderr to an in-memory ring buffer (paged like open-terminal's JSONL `next_offset`); return `{id,status,output,next_offset,…}` |
  | `GET /execute/{id}/status` (`get_process_status`) | drain buffered events since `offset` |
  | `POST /execute/{id}/input` (`send_process_input`) | write to exec stdin (computerd exec accepts a one-shot stdin buffer) |
  | `DELETE /execute/{id}` (`kill_process`) | `stub.shell.killExec(id)` |
  | `GET /execute` (`list_processes`) | list live exec handles for the session |
  | `GET /files/{list,read,grep,glob}` | run against the authoritative dofs `Database` for the session subtree (no sync needed — host is authority) |
  | `POST /files/{write,mkdir,move,delete,replace,upload}` | mutate the authoritative dofs `Database` for the session subtree; subsequent exec sees them via the next push |
  | `GET/POST /files/cwd` | per-session cwd pointer (in-memory map, TTL) |
  | `GET /health`,`/api/config`,`/system`,`/info` | trivial host endpoints; `/api/config` advertises `{terminal:false, notebooks:false, system:true}` (PTY/notebooks deferred) |
  | `/openapi.json` | emit the same operation_ids/shapes so OWUI builds the identical tool surface |

  Deferred (Phase 2+): `/api/terminals` interactive PTY WS, `/notebooks`, `/proxy/{port}`,
  `/ports`, `/files/{archive,view,serve,display}` UI helpers.

- **D10 — AuthN/Z = shared Bearer + required `X-User-Id` + optional `X-Session-Id`.**
  Mirrors open-terminal + k8s-proxy. `Authorization: Bearer $OPEN_TERMINAL_API_KEY` validated
  constant-time at the gateway edge. `X-User-Id` required (else 400). `X-Session-Id` optional;
  when absent, a per-user default session is used (fallback, not rejection — matches the
  k8s-proxy requirement). `X-User-Id`/`X-Session-Id` are trusted plaintext from the Open WebUI
  backend (the same trust assumption open-terminal and k8s-proxy make today).

- **D11 — Session key = `sha256(user)[:N]` + sanitised chat id (one path component).**
  Reuse the k8s-proxy model: `user_hash = sha256(X-User-Id)[:12]`; chat id sanitised to
  `[A-Za-z0-9._-]{1,64}` (reject `/`, `..`, NUL). VFS subtree path =
  `/sess/<user_hash>/<sanitised_chat_or_"default">`. No email/UPN mapping (k8s-proxy confirmed
  none exists there either — `research/03` §7).

- **D12 — Lifecycle: on-demand, idle-reap, caps, in-use pinning, re-adopt on restart.**
  `computer` ships no reaping; we build it. The gateway holds an in-memory `sessions` map keyed
  by subtree path: `{lastActivity, activeExecs, createdAt}`. On first touch of a (user,chat),
  lazily create the subtree (dofs mkdir). A background sweeper (every 60s) evicts idle sessions
  (configurable `SESSION_IDLE_TIMEOUT`, default 30 min): stop any stray execs, delete the VFS
  subtree (or archive it — D4 note). Caps: global `MAX_SESSIONS` + per-user `MAX_SESSIONS_PER_USER`;
  on overflow evict the oldest **idle** session; **never** evict a session with `activeExecs > 0`
  or an open stream. On gateway restart: reopen the PVC SQLite, re-establish the computerd WS,
  and rebuild the `sessions` map from a small metadata table inside the dofs Database
  (`sessions_meta`) so idle timers resume; in-flight execs are lost (client must retry) but files
  survive.

- **D13 — Egress: global default-deny-optional, per-user policy = Phase 2.**
  Phase 1: shared netns, egress governed by the worker Pod's own NetworkPolicy (operator choice).
  Phase 2: per-session netns + an egress proxy/iptables allowlist driven by per-user config
  (replicates k8s-proxy's designed-but-unshipped `user-based-network-policy`). Scope of Phase 1
  explicitly excludes per-user egress.

- **D14 — HA/scale = shard by user-hash across N worker pods.**
  v1 = 1 replica (single dofs SQLite writer). Scale: a stateless router in front dispatches by
  `user_hash % N` to the owning worker pod; each pod owns a disjoint set of users' SQLite files on
  its own PVC. Documented, not built in v1.

## 3. Data-flow walkthroughs

### 3.1 `POST /execute` (the core)

```
OWUI --Bearer+X-User-Id+X-Session-Id--> gateway
  1. verify bearer; resolve (user_hash, chat) -> subtree path P; ensure subtree exists.
  2. cwd = body.cwd || session_cwd || P; resolve under P.
  3. wrapped = nsjailWrap(userCmd, {cwd, uid(u,c), gid(u,c), mem, cpu, time, net: shared});   (D6/D7)
  4. await pushOnce(db, stub.sync);                          // D5: host -> container delta
  5. handle = await stub.shell.exec(wrapped, {cwd: P, env}); // computerd runs sh -c "nsjail …"
  6. pipe handle (ReadableStream<ExecEvent>) into the session's ring buffer under a new id.
  7. on handle.result(): await pullOnce(db, stub.sync);      // D5: container -> host delta
  8. return {id, status:"running"|"completed", output:[...], next_offset, …}   // open-terminal shape
GET /execute/{id}/status?offset=  -> drain buffer since offset.
POST /execute/{id}/input          -> stub.shell.<write stdin>; (computerd exec stdin is one-shot buffer).
DELETE /execute/{id}              -> stub.shell.killExec(id).
```

### 3.2 `GET /files/read` (no exec, no sync)

```
OWUI --> gateway -> resolve path under session subtree P -> dofs Database.readFile(P/path)
  (authoritative store; always consistent; nsjail/jail not involved).
```

### 3.3 `POST /files/write` then `POST /execute`

```
write -> Database.writeFile(P/foo, bytes).          // authoritative updated
execute -> pushOnce ships only the new blob (content-addressed delta) -> exec sees /workspace/foo.
```

## 4. Component inventory (what we write vs reuse)

| Piece | Origin | Est. LOC |
|---|---|---|
| dofs VFS + sync (Database, WorkspaceFilesystem, applyChanges/fetchChanges/pushObjects, watermarks) | **vendored** `packages/dofs/src/**` | — |
| capnweb rpc (server/client, sync-driver pullOnce/pushOnce/tick) | **vendored** `packages/rpc/src/**` | — |
| computerd (FUSE + exec supervisor) | **published image** + nsjail layer | — |
| `FileSQLiteStorage` (node:sqlite on PVC → `DurableObjectStorageLike`) | write (lift from `dofs/testing.ts`) | ~20 |
| WS accept + `/connect` + reverse-dial handler → `WorkspaceRPC` client stub | write | ~80 |
| exec **push/exec/pull** bracket helper | write | ~15 |
| **nsjail command wrapper** + per-session uid/gid allocator + subtree chown | write | ~80 |
| open-terminal REST surface + Zod schemas + `/openapi.json` (Phase-1 ops) | write | ~250 |
| auth (Bearer constant-time) + tenant routing + path sanitisation | write | ~80 |
| lifecycle: sessions map, sweeper, caps, in-use pinning, `sessions_meta`, re-adopt | write | ~120 |
| worker Pod manifest (gateway + computerd sidecar + PVC + privileged) | write (k8s/Helm) | — |
| **total new code** | | **~650** |

## 5. Phase-0 spikes (must validate before build)

1. **nsjail stdio through computerd.** Confirm `stub.shell.exec("nsjail … -- /bin/sh -c 'seq 10000000'")`
   streams stdout with backpressure and that stdin passthrough works, against the real published
   image + a local nsjail. (computerd's runner uses `stdio:[ignore,pipe,pipe]`; nsjail passes
   child stdio through by default — expected OK, but verify.)
2. **dofs gateway-owned authority.** Confirm one Node process can be the single writer of a dofs
   `Database` on a PVC file (WAL) while `computerd`'s FUSE mirror syncs from it over localhost —
   i.e. the DO role reproduced on plain Node. Verify `pullOnce`/`pushOnce` round-trip a write made
   on the host into the container's FUSE view and vice-versa.
3. **`/dev/fuse` + nsjail caps in MicroK8s privileged pod.** Confirm the published computerd
   image mounts FUSE and nsjail can `clone_newuser`/set cgroups under `privileged: true`.

## 6. Configuration (env)

| Var | Default | Meaning |
|---|---|---|
| `OPEN_TERMINAL_API_KEY` (+`_FILE`) | (required) | Bearer token gating all auth'd routes (OWUI sends this) |
| `COMPUTERD_IMAGE` | `ghcr.io/cloudflare/computer-computerd-linux-x64:0.1.0-alpha.1` | sidecar image |
| `VFS_DB_PATH` | `/data/vfs.db` | authoritative dofs SQLite on PVC |
| `COMPUTERD_UPSTREAM_PORT` | `8081` | localhost WS port computerd dials (`/connect {url}`) |
| `SESSION_IDLE_TIMEOUT` | `1800` | seconds idle before a session is reaped |
| `MAX_SESSIONS` | `256` | global concurrent session cap |
| `MAX_SESSIONS_PER_USER` | `16` | per-user session cap |
| `EXEC_TIMEOUT_MS` | `320000` | per-exec wall-clock cap (nsjail `--time_limit` + computerd `timeoutMs`) |
| `EXEC_MEM_MAX_MB` | `1024` | per-exec cgroup mem cap |
| `EXEC_CPU_MS_PER_MS` | `200` (=0.2 core) | per-exec CPU cap |
| `SESSION_STORAGE_QUOTA_MB` | `512` | soft per-subtree storage cap |
| `NETNS_MODE` | `shared` | `shared` (Phase 1) | `isolated` (Phase 2 egress policy) |

## 7. Risks & mitigations

| Risk | Severity | Mitigation |
|---|---|---|
| nsjail stdio/backpressure misbehaves through computerd's pipe runner | High | Phase-0 spike #1; fallback = vendored fork of `computerd/src/exec/runner.ts` to spawn nsjail directly (kept as contingency) |
| dofs single-writer correctness in gateway-owned-authority model | High | Phase-0 spike #2; rely on dofs's own sync invariants; WAL; never let computerd write the PVC SQLite directly |
| Shared kernel ⇒ cross-session escape via kernel/syscall | Medium (accepted) | Threat model = trusted users; nsjail seccomp + dropped caps + ro-rootfs + per-session uid + non-root where possible; document residual risk |
| Single `computerd`/single SQLite ⇒ blast radius + contention | Medium | D14 sharding path; per-user caps; monitor; graceful 503 under overload |
| Interactive PTY (`/api/terminals`) impossible over one-shot exec | Medium | Deferred to Phase 2; for Phase 1 the LLM tool surface (one-shot `/execute`) is unaffected |
| `computer` is `0.1.0-alpha.1` "PREVIEW ONLY" | Low–Medium | Pin the exact image digest; vendor dofs/rpc so we control upgrades; no runtime dependency on Cloudflare APIs |
| `/dev/fuse` + privileged pod hardening | Low | Accept privileged worker (operator decision); scope blast radius via worker NetworkPolicy; long-term consider gVisor/sysbox runtime |
| Port-proxy (`/proxy/{port}`) can't reach a browser | Low | Deferred; feasible later via gateway→computerd `getTcpPort`-equivalent (container shares Pod netns) |

## 8. Explicit non-goals (Phase 1)

- Interactive PTY terminal sessions (`/api/terminals` WS), Jupyter notebooks (`/notebooks`),
  port detection/proxy (`/ports`, `/proxy/{port}`), and file UI helpers
  (`/files/{archive,view,serve,display}`).
- Per-user/per-chat egress network policy (D13 Phase 2).
- HA / multi-replica sharding (D14).
- A process-level "reset/branch chat filesystem" UX (dofs makes this cheap later — not now).
