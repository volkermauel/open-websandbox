# Tasks — use-computerd-as-runtime

Phase 0 de-risks the design; Phases 1–3 build the system; Phase 4 hardens. Each checkbox is
~≤2h of work. Cite `design.md` decision IDs (D#).

## Phase 0 — Spikes (validate before build)

- [ ] **0.1** Clone `computer` vendor set locally: copy `packages/dofs/src/**` and
      `packages/rpc/src/**` into `gateway/vendor/{dofs,rpc}/`; record LICENSE/NOTICE attribution.
- [ ] **0.2** Spike: stand up the published `computerd` image locally (Docker, `--privileged
      --device /dev/fuse`); `POST /connect {"url":"http://host:8081"}`; confirm it reverse-dials
      a `ws` server we run and serves `shell.exec` over it. (`design.md` §5 spike #3)
- [ ] **0.3** Spike: `FileSQLiteStorage` (lift `dofs/testing.ts`, swap `:memory:` → file path),
      open a dofs `Database`, run `WorkspaceFilesystem` mkdir/writeFile/readFile round-trip.
      (`design.md` §5 spike #2 part 1)
- [ ] **0.4** Spike: host-as-authority sync — gateway owns the SQLite, computerd FUSE mirror;
      verify `pushOnce` ships a host-side write into the container's `/workspace` view and
      `pullOnce` captures a container-side `echo > foo` back to the host SQLite.
      (`design.md` §5 spike #2 part 2) **GATE: if this fails, revisit D3/D5 before proceeding.**
- [ ] **0.5** Spike: nsjail command-wrap streaming — `shell.exec("nsjail --mode l --cwd /workspace
      --bind /workspace:/workspace --ro-bind /:/ -- /bin/sh -c 'seq 10000000 | sha256sum'")`;
      confirm stdout streams with backpressure and exit code returns. Install nsjail in a test
      image layered on computerd. (`design.md` §5 spike #1)
- [ ] **0.6** Spike: MicroK8s privileged Pod runs the computerd+nsjail image; `/dev/fuse` mounts;
      nsjail `--clone_newuser` + cgroup mem/cpu limits succeed under `privileged: true`.

## Phase 1 — Vendored foundation + host core

- [ ] **1.1** Finalize vendor layout under `gateway/vendor/`; add a build step (tsc/tsx) that
      compiles dofs+rpc to ESM/CJS without the `cloudflare:workers` re-export entering the graph
      (we only import `dofs` + `rpc`, not `computer`).
- [ ] **1.2** `FileSQLiteStorage` impl + WAL pragma; unit test against dofs schema init.
- [ ] **1.3** `ComputerdSession` class: opens `ws` `WebSocketServer` on `COMPUTERD_UPSTREAM_PORT`,
      accepts computerd's reverse dial, wraps it with `newWebSocketRpcSession` → `WorkspaceRPC`
      client stub (`stub.sync`, `stub.shell`); exposes `connect()`/`health()`.
- [ ] **1.4** `/connect` driver: on container start, `POST http://localhost:8080/connect
      {"url":"http://localhost:<port>"}` (computerd normalises to `ws://…/ws`).
- [ ] **1.5** `execBracketed(stub, db, cmd, opts)`: `await pushOnce; h=shell.exec; await
      h.result(); await pullOnce` (D5). Unit test: write file host-side → exec sees it; exec
      writes → host reads it.
- [ ] **1.6** Health/readiness: gateway `/health`; readiness gated on computerd WS attached +
      SQLite open.

## Phase 2 — Isolation (nsjail) + tenant routing

- [ ] **2.1** `uidAllocator(user, chat)` → deterministic uid/gid in a non-root range; `chown`
      session subtree to match on creation.
- [ ] **2.2** `nsjailWrap(cmd, {cwd, uid, gid, memMb, cpuMsPerMs, timeS})` → command string
      (D6/D7): subtree bind rw, ro-root, tmpfs `/tmp`, `--clone_newuser --uid --gid`, rlimits,
      cgroup mem/cpu, `--time_limit`, seccomp profile, dropped caps. Unit-test the generated
      argv.
- [ ] **2.3** Auth: constant-time Bearer check (`OPEN_TERMINAL_API_KEY`/`_FILE`); `X-User-Id`
      required (400); `X-Session-Id` optional → default fallback (D10).
- [ ] **2.4** Tenant routing: `user_hash=sha256(uid)[:12]`; chat id sanitiser
      `[A-Za-z0-9._-]{1,64}` (reject `/`,`..`,NUL); subtree path `/sess/<u>/<c>`; ensure-on-touch
      mkdir (D11).
- [ ] **2.5** Per-exec resource caps wired from env (`EXEC_TIMEOUT_MS`, `EXEC_MEM_MAX_MB`,
      `EXEC_CPU_MS_PER_MS`); per-subtree soft quota guard on `/files/*` writes
      (`SESSION_STORAGE_QUOTA_MB`).

## Phase 3 — open-terminal REST surface (Phase-1 LLM tools)

- [ ] **3.1** Zod request/response models mirroring open-terminal's Pydantic shapes
      (`ExecRequest`, `WriteRequest`, `ReplaceRequest`, etc.) — `research/02` §2.
- [ ] **3.2** `/execute` (`run_command`): allocate id, `execBracketed`+`nsjailWrap`, stream
      `ExecEvent`s into a per-id ring buffer; return open-terminal response shape with
      `next_offset`. Honor `?wait=` and `?tail=`.
- [ ] **3.3** `/execute/{id}/status` (`get_process_status`), `/execute/{id}/input`
      (`send_process_input`), `DELETE /execute/{id}` (`kill_process`), `GET /execute`
      (`list_processes`).
- [ ] **3.4** `/files/*`: `list_files`, `read_file`, `write_file`, `replace_file_content`,
      `grep_search`, `glob_search`, `mkdir`, `move`, `delete`, `upload_file`, `cwd` get/set — all
      against the authoritative dofs `Database` for the session subtree (no sync).
- [ ] **3.5** `/health`, `/api/config` (`{terminal:false,notebooks:false,system:true}`),
      `/system`, `/info`.
- [ ] **3.6** `/openapi.json` emitting the **identical `operation_id`s + schemas** open-terminal
      exposes, so OWUI builds the same tool surface. Add a conformance test that diffs our
      openapi against open-terminal's.
- [ ] **3.7** End-to-end against a real Open WebUI: register the gateway as an "Open Terminal"
      connection; an LLM `run_command` + `read_file` + `write_file` round-trip succeeds.

## Phase 4 — Lifecycle, deploy, hardening

- [ ] **4.1** `sessions` map + `sessions_meta` table in the dofs DB; on-touch create; lastActivity
      tracking; activeExecs/stream pinning (D12).
- [ ] **4.2** Idle sweeper (60s): evict oldest idle over `MAX_SESSIONS`/`MAX_SESSIONS_PER_USER`;
      never evict in-use; `SESSION_IDLE_TIMEOUT` reap (archive or delete subtree per config).
- [ ] **4.3** Restart re-adopt: reopen PVC SQLite, reconnect computerd WS, rebuild sessions map
      from `sessions_meta`.
- [ ] **4.4** Worker Pod manifest (Helm/Kustomize): gateway + computerd sidecar, shared RWO PVC,
      `securityContext.privileged: true`, `/dev/fuse` device, resource requests/limits, worker
      NetworkPolicy (D8).
- [ ] **4.5** Custom computerd image (`Dockerfile.computerd`): `FROM
      ghcr.io/cloudflare/computer-computerd-linux-x64:0.1.0-alpha.1`, `apt-get install nsjail`
      (+ deps). Pin digest.
- [ ] **4.6** Observability: structured logs (per-session id, user_hash, exec id, durations);
      metrics (active sessions, exec p95, sync bytes, evictions, 503s).
- [ ] **4.7** Security review of the nsjail profile + privilege scope; document residual
      shared-kernel risk in `project.md` / runbook.

## Phase 5 (deferred — out of scope for this change)

- [ ] Interactive PTY `/api/terminals` WS (synthesize persistent shell over one-shot exec; or
      vendored-runner fork). Note: background jobs / TUIs (vim, top) will be degraded.
- [ ] Per-user/per-chat egress network policy (D13 Phase 2).
- [ ] HA: stateless router + shard-by-user-hash across N worker pods (D14).
- [ ] `/notebooks`, `/proxy/{port}`, `/ports`, file UI helpers.
