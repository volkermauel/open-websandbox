# Cloudflare `computer` — Technical Research

Repo: `https://github.com/cloudflare/computer` (cloned locally at `cloudflare/computer`)
Head commit: `63d3636` ("release: stop bundling computerd binary in npm tarball...")
Status: **`0.1.0-alpha.1`**, every package README carries the banner **"PREVIEW ONLY … NOT suitable for production use at this time."**

---

## 1. What it actually is

`@cloudflare/computer` is **a virtual filesystem ("Workspace") that lives inside a Cloudflare Durable Object, plus a set of pluggable execution backends that run commands against that filesystem.** It is *not* a single product called "computer"; "computer" is the umbrella project. The authoritative state is a SQLite-backed VFS held in a Durable Object (DO). Execution is layered on top through `workspace.runtime.exec(...)`.

A "Workspace" can be backed by one of three execution backends (`docs/README.md:6-18`, `docs/16_code_execution.md:16-21`):

- **`container-shell`** — a real Linux container running the `computerd` daemon. Full userland, real binaries, real network. **This is the relevant backend for our use case.**
- **`worker-shell`** — runs `just-bash` (a bash interpreter compiled to JS) inside a Dynamic Worker. No container, instant boot, limited command set.
- **`worker-javascript`** — evaluates an ECMAScript module in a fresh Dynamic Worker with structured I/O.

**A "computer" instance (container backend) = one Durable Object paired 1:1 with one Cloudflare Container running `computerd`** (`docs/11_lifecycle.md:14-18`). It IS built directly on **Cloudflare Containers (the Containers-for-Workers product exposed via `ctx.container` on a container-enabled Durable Object)**, *not* on a generic containerd host or a VM or a Workers isolate. The DO owns the authoritative VFS in SQLite; the container owns a transient FUSE-mounted mirror at `/workspace` and talks to the DO over a long-lived `capnweb` WebSocket.

> The whole thing is explicitly forward-looking preview: "This document describes the design specification … treat intent, not description of code today" (`docs/README.md`).

---

## 2. Architecture & packages

### `packages/`

| Package (npm name) | Role |
|---|---|
| `packages/computer` (`@cloudflare/computer`) | **Core runtime + client SDK.** The top-level `Workspace` API (`fs`, `runtime`, `push`/`pull`, `ready`, `stub`), the container/worker-shell/worker-javascript backends, `WorkspaceProxy`, AI-SDK tools. This is the package a consumer imports. |
| `packages/computerd` (`@cloudflare/computerd`) | The **"injected service" — the daemon that runs *inside* the container.** Owns the FUSE mount, the in-container VFS, the exec runner (`/bin/sh` supervisor), and the HTTP/WebSocket capnweb endpoint the DO dials into. Ships as a Node SEA binary. |
| `packages/computer-computerd-linux-x64` (`@cloudflare/computer-computerd-linux-x64`) | Prebuilt `computerd` SEA binary (linux-x64) + the `ghcr.io/cloudflare/computer-computerd-linux-x64` image, consumed by example Dockerfiles. |
| `packages/dofs` (`@cloudflare/dofs`) | **Storage layer.** Durable-Object SQLite-backed VFS: `Database`, `vfs_*` schema, fs primitives (`mkdir`, `writeFile`, `readFile`, `grep`, …), and the sync-protocol building blocks (`applyChanges`, `fetchChanges`, `pushObjects`, watermarks). |
| `packages/rpc` (`@cloudflare/computer-rpc`) | **Control-plane / wire protocol.** capnweb RPC server+client, the `WorkspaceRPC` bootstrap stub split into `sync` and `shell` sub-stubs. Shared by DO and `computerd`. |

### `examples/`

| Example | Role |
|---|---|
| `examples/container` | Canonical Worker + container-enabled DO hosting `computerd`; exposes `PUT/GET /c/<name>/file/...` and `POST /c/<name>/exec`. |
| `examples/worker-shell` | Same HTTP surface, but shell runs as `just-bash` in a Dynamic Worker (no container). |
| `examples/worker-javascript` | Same shape, but `exec` evaluates an ECMAScript module in a Dynamic Worker. |
| `examples/think` | Minimal `@cloudflare/think` chat agent with a workspace working directory. |
| `examples/think-compare-runtimes` | Web UI running the same agent task on container vs worker runtimes side by side. |
| `examples/tutorial` | Step-by-step: agent writes markdown, runs `pandoc` in the container to produce a PDF. |
| `examples/artifacts` | Generates a Worker project in the workspace and publishes it to Cloudflare Artifacts. |
| `examples/assets` | Prompt → image via Workers AI, written to workspace, shareable link via `@cloudflare/computer/assets`. |

---

## 3. Runtime / isolation model

- **One Workspace instance maps to: one Durable Object + one Cloudflare Container** (`docs/11_lifecycle.md:14-18`). The container is a real Linux sandbox (Debian slim in the canonical image, `examples/container/Dockerfile`), not a VM and not a Workers isolate.
- **Isolation = the container boundary.** It is real Linux with a full POSIX filesystem, real binaries, and real network (`docs/16_code_execution.md:18`: "Full Linux, native binaries, installed packages, processes"). There is **no per-instance kernel** — containers run on Cloudflare's shared containerd host. Network/filesystem/user namespaces are whatever the Containers-for-Workers platform provides (the package does not configure them itself).
- **Filesystem model: split-store.** Authoritative VFS = SQLite inside the DO (`ctx.storage`); the container holds an **in-memory mirror** exposed via a FUSE mount at `/workspace` and synced back over capnweb (`docs/11_lifecycle.md:33-36`). Writes inside the container are captured by the FUSE driver and pushed back to the DO. The container's in-memory VFS is **lost on container restart** and rebuilt from the DO store (`packages/computerd/README.md`: "No on-disk persistence yet — in-memory VFS rebuilt on start").
- **Runs as `root`** by default — documented as a poor-isolation gap, with unprivileged exec planned (`docs/07_injected_service.md:226-235`).
- **Resource limiting:**
  - *Container-level:* via Cloudflare Containers config in `wrangler.jsonc` — `examples/container/wrangler.jsonc` sets `instance_type: "standard-2"` and `max_instances: 5`. There is **no per-exec CPU/mem/disk knob** passed through the SDK (`container.start({ enableInternet, env })` at `packages/computer/src/backends/container/container-host.ts:122,124,142`).
  - *Exec-level:* `timeoutMs` (default **320 s**, `packages/computerd/src/exec/runner.ts:59`), SIGTERM→SIGKILL grace 5 s (`runner.ts:62`), and `EXEC_LOG_MAX_BYTES` (default **16 MB** retained stdout/stderr log, `runner.ts:56`).
  - *Storage:* DO SQLite cap is "~10GB maximum" (`packages/computer/README.md` limitations). The JS-isolate backend additionally caps concurrent executions (default 24), stdio bytes, capability calls, etc. (`docs/17_isolate_javascript.md:84-86`).

---

## 4. Lifecycle & API surface

**CREATE an instance.** A Workspace is constructed inside a DO; the container is started lazily on first `ready()`. Creation is an SDK/DO-RPC call, not a config file or CLI:

```ts
// packages/computer/README.md:102-118
class MyDO extends DurableObject<Env> {
  #workspace = new Workspace({
    storage: this.ctx.storage,            // DO VFS backed by SQLite
    backends: [
      new CloudflareContainerBackend({
        container: { binding: "Agent", id: this.ctx.id.toString() },
      }),
    ],
  });
  async initialize() {
    await this.workspace.ready();
    await this.workspace.fs.mkdir("/workspace", { recursive: true });
  }
}
```

The actual container launch is `container.start({ enableInternet: true, env })` — `container-host.ts:122-124`:

```ts
// packages/computer/src/backends/container/container-host.ts:122-126
      this.#container.start({ enableInternet: true, env });
    } else if (!this.#container.running) {
      this.#container.start({ enableInternet: true, env });
    }
    installContainerMonitor(this.#ctx, this.#container);
```

`start()` is idempotent (returns once the platform accepts the command); readiness is then verified by probing `GET /health` on the container's port (`cloudflare-container.ts:400-423`).

**EXECUTE a command.** One execution router, `workspace.runtime.exec(source, opts)` (`docs/05_runtime_interface.md:5-15`):

```ts
const handle = await workspace.runtime.exec("ls -la /workspace", { cwd: "/workspace" });
const result = await handle.result();
```

It is **one-shot exec, not a persistent PTY/session.** Each `exec()` spawns a fresh `/bin/sh -c "<command>"` child (`packages/computerd/src/exec/runner.ts:145`):

```ts
// packages/computerd/src/exec/runner.ts:144-148
    const wrapped = cwd !== undefined ? `cd ${shellQuote(cwd)} && ${command}` : command;
    const child = spawn("/bin/sh", ["-c", wrapped], {
      env,
      stdio: [options.stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"],
    });
```

There is **no interactive shell, no PTY, no environment that persists across `exec` calls** beyond what lives in the filesystem. `stdin` is accepted as a one-shot buffer (`runner.ts:149-155`), not a streaming TTY. The full API: `exec`, `getExec(id, {after})` for reattach, `killExec`, `disposeExec` (`docs/05_runtime_interface.md:20-25`).

**STREAM output.** The handle is itself a `ReadableStream` of `{id, seq, name, value}` events where `name ∈ {stdout, stderr, exit, heartbeat}` (`runner.ts:38-44`, `docs/05_runtime_interface.md:38-44`). Events flow over the capnweb WebSocket and are commonly re-framed as SSE on the Worker edge (`packages/computer/README.md:182-208`):

```ts
// packages/computer/README.md:182-199  (SSE framing of the exec stream)
  const run = await this.workspace.runtime.exec("npm install", { ... });
  const sse = run.pipeThrough(
    new TransformStream<
      | { id: string; seq: number; name: "stdout" | "stderr"; value: string }
      | { id: string; seq: number; name: "exit"; value: number },
      Uint8Array
    >({
      transform(event, controller) {
        const frame = `event: ${event.name}\ndata: ${JSON.stringify(event.value)}\n\n`;
        controller.enqueue(new TextEncoder().encode(frame));
      },
    }),
  );
```

Consumer side (`README.md:214-217`): an `EventSource` listening for `stdout`/`stderr`/`exit` events.

**DESTROY / expiry.** `container.destroy()` tears the container down (`container-host.ts:135-141`). Auto-expiry is delegated to "Cloudflare Containers' own lifetime policy" — the package does not implement its own idle TTL (`docs/11_lifecycle.md:110-114`). **DO hibernation is NOT yet implemented**: today the code uses `server.accept()` rather than `ctx.acceptWebSocket()`, so an open Workspace keeps the DO warm indefinitely (`docs/11_lifecycle.md:282-304`).

**Persistence.** FS state is durable *in the DO's SQLite* across DO and container restarts; the sync protocol rebuilds the container's mirror from `_vfs_watermark` cursors (`docs/11_lifecycle.md:402-406`). The container itself has **no on-disk persistence** (`packages/computerd/README.md`). Working directory is pinned per-exec via `cwd`. Read-only volumes can be mounted: R2 buckets (`examples/container/src/index.ts:75-77`) and, via `@cloudflare/computer/git`/`assets`, git/artifact mounts.

**Snapshotting / image customization.** Yes — you build a custom base image. The canonical recipe (`packages/computer/README.md:65-83`) `COPY`s the prebuilt `computerd` binary from the GHCR image into a thin Debian base, so you can `apt-get install` anything you need into the image. The image is referenced from `wrangler.jsonc` `containers[].image`.

---

## 5. Networking

**Inbound (how you reach a running computer):** you do **not** connect to the container directly. The path is `client → Worker HTTP → DO (Workers RPC) → container` (`examples/container/src/index.ts:10-18`). The Worker is the only public surface. To reach the container's ports, the DO uses `container.getTcpPort(port).fetch(...)` (`container-host.ts:167-178`). The example exposes only a file/exec HTTP surface, not raw ports.

**The reverse-tunnel / connectivity model.** The container **dials out** to the DO over a WebSocket — the DO is the WebSocket *server*, `computerd` is the *client*. This inversion is deliberate: the DO calls `container.interceptOutboundHttp(egressHost, fetcher)` so the container's egress is routed through a DO-controlled `Fetcher`, then `POST /connect` tells `computerd` to dial `ws://computer.internal/ws` back to the DO (`docs/07_injected_service.md:116-142`, `cloudflare-container.ts:199`). This is how the DO controls routing and keeps the WebSocket carrier on its own path.

**Outbound / egress.** Two levers:

- `enableInternet` (passed `true` in all shipped start paths, `container-host.ts:122,124,142`) — controls whether the container can reach the public internet.
- `container.interceptOutboundHttp(host, fetcher)` — routes the container's outbound HTTP at `host` back into a DO-owned `Fetcher` (`container-host.ts:154-165`), i.e. an egress-interception/allowlist point.

So you *can* lock down or mediate egress, but the shipped default gives the container live internet.

---

## 6. AuthN / AuthZ

- **Authentication of API calls** is via **Cloudflare account bindings** — your Worker invokes the DO through `env.ContainerExample.get(env.ContainerExample.idFromName(name))` (`examples/container/src/index.ts:177,229`). There is **no API-token model, no per-caller identity, and no tenant model inside the package.** It is a single-owner system: whoever owns the Cloudflare account + Worker owns every Workspace.
- **The capnweb RPC handshake is unauthenticated today.** It is "safe on Cloudflare Containers because only the owning DO [can] reach the container's TCP port" (`docs/07_injected_service.md:216-225`). The `EAUTH` error code is *reserved* but never raised (`docs/08_capnweb_interface.md:317`). A hello/auth phase is explicitly listed as deferred future work.
- **Backend selection is routing, not authorization** — `docs/05_runtime_interface.md:89`: "Omitting `backend` selects the first configured backend. Backend selection is routing, not authorization; **public gateways must validate it against server-side policy.**" (`docs/16_code_execution.md:66-86` repeats this: "The backend argument is never itself authorization.")
- **Multi-tenancy is the consumer's job.** The pattern for per-user/per-chat isolation is to mint one DO per tenant (e.g. `idFromName("user-123")`, `docs/11_lifecycle.md:251-256`) and gate the Worker's own HTTP endpoint yourself. The cross-DO "container pool" shape (`container-host.ts:23-34`) shows how a pool-member DO can own containers on behalf of agent DOs.

---

## 7. Language / SDK

**TypeScript / JavaScript**, confirmed across all `package.json` (no other language). Target runtime: Workers + Durable Objects on the host side; Node 22+ SEA binary (`computerd`) on the container side. The client SDK is `@cloudflare/computer`, consumed as Workers-TS subpath imports:

```ts
import { Workspace, withWorkspace, ... } from "@cloudflare/computer";
import { CloudflareContainerBackend } from "@cloudflare/computer/backends/container";
```

**Representative snippets (verbatim):**

Create an instance (DO + backend wiring) — `examples/container/src/index.ts:51-93`:

```ts
class ContainerBase extends withWorkspaceContainer(class extends DurableObject<Env> {}) {
  readonly backend = new CloudflareContainerBackend({
    container: () => this,
    workspace: { binding: "ContainerExample", id: this.ctx.id.toString() },
  });
}
export class ContainerExample extends withWorkspace(ContainerBase, workspaceOptions) {
  override async fetch(request: Request): Promise<Response> {
    return this.backend.handleFetch(request);
  }
}
```

Run a command and read the buffered result — `examples/container/src/index.ts:232-233`:

```ts
    const handle = await ws.runtime.exec(command, { cwd: body.cwd, encoding: "utf8" });
    const result = await handle.result();
```

Read streamed output as SSE — `packages/computer/README.md:185-208`:

```ts
  const run = await this.workspace.runtime.exec("npm install", { ... });
  const sse = run.pipeThrough(
    new TransformStream<...>({ transform(event, controller) {
      const frame = `event: ${event.name}\ndata: ${JSON.stringify(event.value)}\n\n`;
      controller.enqueue(new TextEncoder().encode(frame));
    } }),
  );
  return new Response(sse, { headers: { "content-type": "text/event-stream", ... } });
```

The correct lifecycle pattern on the Worker side uses `using` to dispose RPC stubs (`docs/11_lifecycle.md:249-262`):

```ts
  const id = env.COMPUTERD.idFromName("user-123");
  using ws = await env.COMPUTERD.get(id).getWorkspace();
  using handle = await ws.runtime.exec("npm test");
  const result = await handle.result();
```

---

## 8. Pricing / product dependency

**Hard product dependencies (container backend):**

1. **Cloudflare Containers** (Containers for Workers), via a container-enabled Durable Object's `ctx.container`. `container.start({enableInternet, env})`, `getTcpPort`, `interceptOutboundHttp`, `destroy`, `monitor` are all Containers-platform primitives (`packages/computer/src/backends/container/container-host.ts`). The `wrangler.jsonc` declares a `containers[]` block (`examples/container/wrangler.jsonc`).
2. **Durable Objects (SQLite-backed)** — the authoritative VFS lives in `ctx.storage` (`docs/11_lifecycle.md:14-18`).
3. (Optional) **R2** for read-only mounts; **Cloudflare Artifacts** binding for the `artifacts` subpath; **Worker Loader** (`env.LOADER`) for the worker-shell/worker-javascript backends.

**Cost-relevant notes from the repo:**

- "It costs a container per session and a real roundtrip on every filesystem op." (`docs/12_worker_backend.md:26-28`).
- "You're sensitive to the per-session container cost." — the stated reason to prefer the worker-shell backend (`docs/12_worker_backend.md:48`).
- "~10GB maximum [storage] shared across [the DO]" (`packages/computer/README.md`).
- `max_instances` caps total concurrent containers (example sets 5).

**No dollar pricing, region list, or SLA** appears anywhere in the repo — that lives in Cloudflare's external Containers/DO pricing pages, which is out of scope here. The package is **`0.1.0-alpha.1` / PREVIEW ONLY** across the board, so any quotas are preview quotas.

---

## 9. Gaps / gotchas

- **PREVIEW ONLY, explicitly not production-ready** — banner on `README.md`, `packages/computer/README.md`, `packages/computerd/README.md`, `packages/dofs/README.md`. "APIs unstable; design subject to change."
- **DO hibernation is not implemented** — uses `server.accept()`, so an open Workspace keeps the DO (and thus billing memory) warm indefinitely; this is "deferred work" (`docs/11_lifecycle.md:282-304`, `358-372`).
- **No on-disk persistence in the container** — the in-memory VFS is rebuilt on container restart; only the DO-side SQLite is durable (`packages/computerd/README.md`, `docs/07_injected_service.md:205-209`).
- **RPC handshake is unauthenticated** — safe only because Containers network-isolate each DO's container. "The moment [we] support providers with broader network exposure [the] server needs its own auth on RPC handshake." (`docs/07_injected_service.md:216-225`). `EAUTH` is reserved but unused.
- **Single-writer, 1:1 DO:container** — "One agent, one container, one DO. No concurrent writers." (`docs/02_sync_protocol.md:359`). Concurrent containers on one Workspace are not supported.
- **No persistent shell / PTY / session** — `exec()` is one-shot `/bin/sh -c`. Cross-command environment (env vars, cwd, background processes) does **not** persist between calls except via the filesystem. An interactive-REPL UX would have to be built on top (one exec per command, or a long-lived exec acting as a server).
- **Runs as `root`** in the container by default — unprivileged process user is planned, not shipped (`docs/07_injected_service.md:226-235`).
- **Backend selection ≠ authorization** — a public endpoint must enforce per-caller policy itself (`docs/05_runtime_interface.md:89`, `docs/16_code_execution.md:66-86`).
- **No per-exec CPU/mem/disk limits** — only the platform-level `instance_type`/`max_instances` and the exec-level `timeoutMs`/log-cap. Resource isolation between tenants is whatever Containers-for-Workers gives you, not anything this package configures.
- **`worker-shell` can't reattach/dispose** — for supervised/detached process lifecycle you must use `container-shell` or `worker-javascript` (`docs/05_runtime_interface.md:109`, `docs/16_code_execution.md:62-64`).
- **Path confinement is lexical, not inode-atomic** — do not treat one isolate capability as a security boundary against a concurrent privileged principal rewriting paths (`docs/17_isolate_javascript.md:178`).
- **Container lifetime is opaque** — reaped by "Cloudflare Containers' own lifetime policy"; the package surfaces exit via `monitor()` but does not control the TTL (`docs/11_lifecycle.md:110-114`).

---

## TL;DR

- **What it is:** an alpha-stage (`0.1.0-alpha.1`, "PREVIEW ONLY") library that puts a SQLite-backed virtual filesystem in a **Cloudflare Durable Object** and runs commands against it via pluggable backends — the flagship being a **real Linux container running the `computerd` daemon**.
- **Hard dependencies:** **Cloudflare Containers** (Containers for Workers, via `ctx.container`) **+ Durable Objects (SQLite)**. A container backend = "one container per session" cost; there is no dollar pricing in-repo.
- **Exec model = one-shot, not interactive.** Each `workspace.runtime.exec(cmd)` spawns a fresh `/bin/sh -c`, streams `{stdout,stderr,exit}` over a WebSocket/SSE, and is killable/reattachable by id. **There is no persistent PTY, no surviving shell session, no cross-call env** — state survives only in the filesystem.
- **Isolation & resources:** real container boundary, real network, runs as **root** by default; no per-exec CPU/mem limits (only platform `instance_type`/`max_instances` + per-exec timeout 320 s / 16 MB log cap).
- **Networking:** no direct container ingress — clients hit a **Worker → DO → container** path; the container **reverse-dials** the DO (`POST /connect` → `ws://computer.internal/ws`) and egress is interceptable via `container.interceptOutboundHttp`.
- **Auth/multi-tenancy:** none built in — single-owner (your Cloudflare account/Worker); the RPC handshake is **unauthenticated**; per-user isolation is **your responsibility** (mint one DO per user/chat and gate the edge yourself).
- **Bottom line for open-terminal:** architecturally viable (per-user DO + container, streaming exec over SSE, durable FS), but you'd be building your own auth, per-tenant policy, idle-reaping, and likely an interactive-session shim on top of an **explicitly preview, unstable, non-production** foundation that currently has no DO hibernation and no persistent shell.
