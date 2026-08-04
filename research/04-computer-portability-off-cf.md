# Reusing Cloudflare `computer` off-Cloudflare — Portability Assessment

Repo investigated: `cloudflare/computer` (github.com/cloudflare/computer), MIT.
Goal: a portable per-session command-execution runtime on plain Docker / Kubernetes / containerd,
with **no** Cloudflare-proprietary dependencies (`ctx.container`, `ctx.storage`, Durable Objects,
Workers runtime, Cloudflare account).

Key finding up front: the three foundation packages — **`dofs`** (SQLite VFS + sync),
**`rpc`/capnweb** (transport-agnostic RPC), and **`computerd`** (in-container FUSE + exec daemon) —
contain **zero** `cloudflare:` imports and **zero** references to `ctx.storage` / `ctx.container` /
`DurableObject` / `WebSocketPair`. They run on plain Node 22 + `node:sqlite` + `ws` + `fuse-native`.
The Cloudflare coupling is confined to `packages/computer` (the host-side facade), and within it
almost entirely to **3 files** under `packages/computer/src/backends/container/`.

---

## 1. `computerd` portability — REUSABLE-AS-IS

`computerd` is a Node SEA binary. Its only runtime dependencies (`packages/computerd/package.json`)
are `@cloudflare/computer-rpc`, `@cloudflare/dofs`, `@platformatic/vfs`, `fuse-native` — all plain
Node packages. The entrypoint (`packages/computerd/src/cli/computerd.ts`) imports only `node:*`,
`ws`, and the two `@cloudflare/*` workspace packages:

```ts
// packages/computerd/src/cli/computerd.ts:3-17
import { mkdir } from "node:fs/promises";
import { createServer, ... } from "node:http";
import type { Socket } from "node:net";
import { isAbsolute } from "node:path";
import type { ExecEvent as RpcExecEvent } from "@cloudflare/computer-rpc";
import { createWorkspaceClient, type WorkspaceClient } from "@cloudflare/computer-rpc/client";
...
import type { Database } from "@cloudflare/dofs";
import { WebSocket, WebSocketServer } from "ws";
import { Runner } from "../exec/index.js";
...
import { createWorkspaceServer, acceptWebSocketSession, serveHTTPBatch } from "@cloudflare/computer-rpc/server";
```

A repo-wide grep confirms the absence of Cloudflare-only APIs in the daemon:

```
$ grep -rn "cloudflare:" packages/computerd/src            # → (empty)
$ grep -rn "ctx.storage|ctx.container|DurableObject" packages/computerd/src packages/dofs/src packages/rpc/src
# → only FUSE env vars (COMPUTERD_FUSE_*) and the dofs test "DurableObjectStorageLike" *interface name*
```

The exec supervisor is pure Node (`packages/computerd/src/exec/runner.ts:18-19`):

```ts
import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
```

**Runtime requirements** are exactly: (a) a FUSE-capable kernel (`/dev/fuse`) or the userspace
`shim` fallback (`FUSE_MOUNT=shim|none|auto`), (b) Node 22 (baked into the SEA), (c) a reachable
address to dial back to. Nothing Cloudflare-specific.

### The connect / reverse-WS handshake — fully host-driven

`computer.internal` is **not** a hardcoded hostname inside `computerd`. The dial-back target is
supplied by whoever POSTs to `/connect`. `computerd`'s handler takes a URL from the request body,
normalizes `http(s)→ws(s)`, appends `/ws`, and serves its RPC over the dialed socket:

```ts
// packages/computerd/src/cli/computerd.ts:208-218
    if (path === "/connect") {
      if (request.method !== "POST") { send(response, 405, ...); return; }
      void handleConnect(request, response, rpc, upstreamSlot);
      return;
    }
```

```ts
// packages/computerd/src/cli/computerd.ts:325-333  (body shape)
interface ConnectBody {
  // Base URL of the egress endpoint. ws[s]:// or http[s]://; we
  // normalise http(s) to ws(s) and append /ws.
  url?: unknown;
  healthTimeoutMs?: unknown;
}
```

```ts
// packages/computerd/src/cli/computerd.ts:386-392
  const wsUrl = `${toWebSocketUrl(baseUrl)}/ws`;
  const ws = new WebSocket(wsUrl);
  upstreamSlot.ws = ws;
  ws.once("open", () => {
    console.log(`/connect: attached RPC session to ${wsUrl}`);
    acceptWebSocketSession(ws, rpc);            // ← computerd SERVES rpc over the dialed socket
  });
```

```ts
// packages/computerd/src/cli/computerd.ts:416-421
function toWebSocketUrl(input: string): string {
  if (input.startsWith("ws://") || input.startsWith("wss://")) return input;
  if (input.startsWith("http://")) return `ws://${input.slice("http://".length)}`;
  if (input.startsWith("https://")) return `wss://${input.slice("https://".length)}`;
  throw new Error(`unsupported URL scheme: ${input}`);
}
```

So a plain Node host simply POSTs `{"url":"http://<host>:<port>"}` to `computerd:8080/connect` and
`computerd` dials `ws://<host>:<port>/ws`. We can point it at **our** WebSocket server.

There is also an alternate client mode: if env `UPSTREAM_URL` is set, `computerd` itself opens a
client sync session (pull/push) against that URL — useful if the host wants `computerd` to be the
sync driver rather than the responder:

```ts
// packages/computerd/src/cli/computerd.ts:467-481
  const upstreamUrl = process.env.UPSTREAM_URL?.trim();
  let upstreamClient: WorkspaceClient | undefined;
  if (upstreamUrl !== undefined && upstreamUrl.length > 0) {
    upstreamClient = createWorkspaceClient({
      url: upstreamUrl,
      WebSocketImpl: WebSocket as unknown as typeof globalThis.WebSocket,
    });
  }
  const { vfs, db, stopSync } = await createNodeVirtualFileSystem({
    upstream: upstreamClient?.sync,
  });
```

**Conclusion:** `computerd` runs unmodified in a plain Linux container. `computer.internal` lives
only on the **Cloudflare side** as a default (`packages/computer/src/backends/container/cloudflare-container.ts:132`,
`const DEFAULT_EGRESS_HOST = "computer.internal";`, overridable via the `egressHost` option at
`:84`). We supply our own URL.

---

## 2. `dofs` portability — REUSABLE-AS-IS

`dofs` is a SQLite-backed VFS plus a content-addressed sync protocol. Its storage seam is a
**3-method interface** with no Durable-Object coupling:

```ts
// packages/dofs/src/types.ts:5-16
export interface SQLStorageLike {
  exec<Row extends object = Record<string, unknown>>(
    query: string,
    ...bindings: unknown[]
  ): SQLCursorLike<Row>;
}

export interface DurableObjectStorageLike {
  sql: SQLStorageLike;
  transaction?<T>(closure: () => T | Promise<T>): T | Promise<T>;
  transactionSync?<T>(closure: () => T): T;
}
```

The `Database` class wraps that interface (and normalizes DO's `ArrayBuffer` blobs ↔ node's
`Uint8Array`):

```ts
// packages/dofs/src/storage.ts:3-13
export class Database {
  readonly sql: SQLStorageLike;
  readonly transactionSync: <T>(closure: () => T): T;
  ...
  constructor(storage: DurableObjectStorageLike) {
    this.sql = storage.sql;
    this.transactionSync = <T>(closure: () => T): T => { ... };
  }
```

A complete Node implementation already ships, backed by the built-in **`node:sqlite`** (no native
addon, no Cloudflare):

```ts
// packages/dofs/src/testing.ts:14, 33-58
import { DatabaseSync, type StatementSync } from "node:sqlite";
...
export class SQLiteTestStorage implements DurableObjectStorageLike {
  private readonly db: DatabaseSync;
  ...
  constructor() {
    this.db = new DatabaseSync(":memory:");          // ← swap ":memory:" for a volume path
    this.sql = {
      exec: <Row ...>(query, ...bindings) => { ... stmt.all(...) ... return new TestCursor<Row>(rows); },
    };
  }
  transactionSync<T>(closure: () => T): T { this.db.exec("BEGIN"); ... this.db.exec("COMMIT") ... }
}
```

The workerd variant (`packages/dofs/src/fs/with-db.workers.ts:29`) shows the **only** place a real
DO is touched — and it's just `state.storage` cast to the same interface:

```ts
// packages/dofs/src/fs/with-db.workers.ts:28-31
  return runInDurableObject(stub, async (_instance: unknown, state: DurableObjectState) => {
    const db = new Database(state.storage as unknown as DurableObjectStorageLike);
    initializeSchema(db, options.now ?? (() => 1000));
```

**To back the VFS with a local SQLite file on a volume**, copy `SQLiteTestStorage` and change one
line — `new DatabaseSync(":memory:")` → `new DatabaseSync("/data/session.db")`. `better-sqlite3`
would also satisfy the interface with a ~15-line adapter. `node:sqlite` is built into Node 22,
which is what `computerd` already requires (`packages/computer-computerd-linux-x64/README.md`,
"Node 22 or newer").

### Sync-protocol entry points a host must call

Exported from `@cloudflare/dofs` (`packages/dofs/src/index.ts:38-58`). These are pure functions
over `Database` — a host wires them to a `SyncRPC` transport stub (next section):

| Function | Signature (abridged) | Source |
|---|---|---|
| `applyChangesSync` | `(db, entries, blobs, opts) => void` | `sync/apply.ts` |
| `coalesceChanges` | `(db, after, opts) => AsyncIterable<ChangeEntry>` | `sync/coalesce.ts` |
| `fetchChanges` | `(db, after, opts) => AsyncIterable<ChangeEntry>` | `sync/fetch.ts` |
| `fetchObjects` | `(db, hashes) => AsyncIterable<{hash,bytes}>` | `sync/fetch.ts` |
| `hasObjects` | `(db, hashes) => Uint8Array[]` | `sync/fetch.ts` |
| `pushObjects` | `(db, stream) => Promise<void>` | `sync/push.ts` |
| `stageBlob` | `(db, hash, bytes, now) => void` | `sync/blobs.ts` |
| `materialiseChange` | `(db, path) => ChangeEntry \| null` | `sync/changes.ts` |
| `currentRev` / `readWatermark` / `writeWatermark` / `readFetchCursor` / `writeFetchCursor` | watermark cursors | `sync/watermarks.ts` |

A host does **not** call these directly in the happy path — the `SyncRPC` server (`rpc/server.ts`)
binds them to a `Database`, and the `sync-driver` (`pullOnce`/`pushOnce`/`tick`) drives the wire.

---

## 3. `rpc` (capnweb) portability — REUSABLE-AS-IS

`capnweb` is transport-agnostic: it speaks over any WHATWG-shaped WebSocket. The RPC package's
only runtime deps are `@cloudflare/dofs` and `capnweb` (`packages/rpc/package.json`); `ws` is a
devDependency used by tests and by `computerd`. **No `cloudflare:` imports** anywhere in
`packages/rpc/src`.

A Node process can be the capnweb **server** that `computerd` dials. The server factory takes a
`Database` + a `RunnerLike` and returns the composite stub; `acceptWebSocketSession` binds it to a
plain `ws`-package socket:

```ts
// packages/rpc/src/server.ts:326-332
export function createWorkspaceServer(
  db: Database,
  runner: RunnerLike,
  options: ServerOptions = {},
): WorkspaceRPC {
  return new WorkspaceRPCServer(createSyncServer(db, options), createShellServer(runner));
}
```

```ts
// packages/rpc/src/server.ts:344-349
export function acceptWebSocketSession(
  ws: WebSocket | { addEventListener: WebSocket["addEventListener"] },
  rpc: SyncRPC | ShellRPC | WorkspaceRPC,
): void {
  newWebSocketRpcSession(ws as unknown as WebSocket, rpc as unknown as RpcTarget);
}
```

The comment is explicit that the node `ws` server socket is supported
(`packages/rpc/src/server.ts:335-337`): *"the node `ws` package's server-side sockets implement the
WHATWG surface (addEventListener / send / close), so this works for both browser-style sockets and
ws-package sockets."*

The matching **client** (host dials `computerd`, or `computerd` dials host) is one call:

```ts
// packages/rpc/src/client.ts:136-147
export function createWorkspaceClient(options: { url: string; WebSocketImpl?: typeof WebSocket; }): WorkspaceClient {
  const WS = options.WebSocketImpl ?? WebSocket;
  const ws = new WS(options.url);
  const stub = newWebSocketRpcSession(ws as unknown as globalThis.WebSocket) as RpcStub<WorkspaceRPC>;
  ...
```

### The `WorkspaceRPC` bootstrap stub — what a host must implement

A host does **not** implement `WorkspaceRPC` itself; it obtains one as an RPC **client** stub
pointing at `computerd` (via `createWorkspaceClient` or by accepting `computerd`'s reverse dial and
wrapping it with `newWebSocketRpcSession`). The contract a host drives is (`packages/rpc/src/interface.ts:138-141`):

```ts
export interface WorkspaceRPC {
  sync: SyncRPC;
  shell: ShellRPC;
}
```

The `sync` sub-stub (`packages/rpc/src/interface.ts:22-88`) — `push`, `fetchChanges`, `watermarks`,
`readEntry`, `hasObjects`, `fetchObjects`, `pushObjects`. The `shell` sub-stub
(`:93-133`) — `exec`, `getExec`, `killExec`, `disposeExec`. If a host instead wants to **serve**
authoritative state to `computerd` (the DO role), it implements `SyncRPC` — and `rpc/server.ts`
already does that (`SyncRPCServer`, `:89-228`) over a local `Database`, so again nothing to write.

---

## 4. The `Workspace` host coupling — the SEAM

`packages/computer` is the host-side facade. The `Workspace` class **itself** is transport- and
storage-agnostic — its only storage contract is the dofs interface:

```ts
// packages/computer/src/workspace.ts:80-85
export interface WorkspaceOptions {
  // Local store backing Workspace. In Durable Object, pass
  // `ctx.storage`; in tests, pass SQLiteTestStorage
  // @cloudflare/dofs/testing. constructor opens
  // Database against runs initializeSchema (idempotent).
  storage: DurableObjectStorageLike;
```

**One caveat for reuse:** the package's main entry re-exports `WorkspaceProxy`/`WorkspaceServiceProxy`
from `./proxy.js`, which imports `cloudflare:workers`:

```ts
// packages/computer/src/index.ts:53-59
export {
  ArtifactsCLITarget,
  WorkspaceProxy,
  type WorkspaceProxyProps,
  WorkspaceServiceProxy,
  type WorkspaceServiceProxyProps,
} from "./proxy.js";
```

```ts
// packages/computer/src/proxy.ts:51
import { RpcTarget, WorkerEntrypoint } from "cloudflare:workers";
```

So `import { Workspace } from "@cloudflare/computer"` **will fail to load in plain Node** because
`cloudflare:workers` is unresolvable outside workerd. Two clean fixes: (a) vendor `workspace.ts` +
its non-CF deps into the host, or (b) don't use the `Workspace` facade at all — drive `dofs` +
`rpc` directly (see §7). The `runtime/runtime.ts` imports only types (`:1-4`); the `cloudflare:`-importing
`runtime/bridge.ts` is referenced **only** by the `worker-javascript` backend, not the container path.

### The CF API surface that makes the container backend Cloudflare-only

Repo-wide grep (`grep -rn "from \"cloudflare:\"" packages/computer/src --include=*.ts | grep -v test`)
narrows the non-test coupling to: `backends/container/container-host.ts`, `backends/container/cloudflare-container.ts`,
`backends/worker-javascript/*`, `backends/worker-shell/entrypoint.ts`, `proxy.ts`, `runtime/bridge.ts`.
For the **container** use case only the first two (+ sibling `container-lifecycle.ts`) matter.

| CF API used | where (file:line) | what it does | portable equivalent we'd build |
|---|---|---|---|
| `import { RpcTarget } from "cloudflare:workers"` | `backends/container/container-host.ts:21` | makes `WorkspaceContainerAPI` passable over Workers RPC | drop; a plain class is fine — host and container talk over WebSocket, not Workers RPC |
| `ctx.container` (`DurableObjectState.container`) | `container-host.ts:95,103` | the Containers-for-Workers handle | Docker SDK `container`/`start`, or k8s Pod create, or CRI |
| `ctx.container.start({ enableInternet, env })` | `container-host.ts:122,124,142` | launch/relaunch the container with env | `docker.containers.create+start` with `Env`; k8s Pod `create` |
| `ctx.container.running` | `container-host.ts:123,147` | liveness flag | poll container state / Pod phase |
| `ctx.container.getTcpPort(n).fetch(...)` | `container-host.ts:167-178,181` | fetch into a container TCP port | dial `computerd` directly (we already expose its HTTP port) — no port-forward needed |
| `ctx.container.interceptOutboundHttp(host, fetcher)` | `container-host.ts:154-165` | rewrite container DNS so `computer.internal` routes back to the DO | set `computerd`'s dial URL via `/connect` body instead — **no interception needed** |
| `ctx.container.destroy()` (+ `destroyContainerExpectingExit`) | `container-lifecycle.ts` (via `container-host.ts:117,136`) | tear down generation | `docker.containers.kill/remove`; k8s Pod `delete` |
| `ctx.waitUntil` / `DurableObjectState` monitor | `container-lifecycle.ts:50,64,91,123` | background exit monitoring | a `setInterval`/event watch in the Node host |
| `new WebSocketPair()` + `server.accept()` | `cloudflare-container.ts:297-299` | accept `computerd`'s reverse `/ws` dial | a `ws.WebSocketServer` `connection` event + `newWebSocketRpcSession(ws)` — exactly what `computerd.ts:288-301` already does |
| `env.<BINDING>` (DO namespace refs) | `examples/container/src/index.ts:177,229`; `cloudflare-container.ts` options | resolve the Workspace-owning DO | not applicable — the host **is** the owner; hold the `WorkspaceRPC` stub in a Map keyed by session id |
| `ctx.storage` (DO SQL) | `workspace.ts:85` doc; `with-db.workers.ts:29` | authoritative VFS store | file-backed `node:sqlite` via `SQLiteTestStorage` (§2) |

The seam is small and crisp: a portable host must provide **(1) container lifecycle** (start/stop a
container from an image), **(2) a WebSocket server** that accepts `computerd`'s reverse dial and
wraps it as a `WorkspaceRPC` client stub, and **(3) a SQLite-backed `Database`** for authoritative
state. Everything else is reused verbatim.

---

## 5. `CloudflareContainerBackend` — what's CF-specific vs. plain-container equivalents

What the backend orchestrates in `connect()` (`packages/computer/src/backends/container/cloudflare-container.ts:178-283`):

1. **Resolve container host** (`:182-183`) — `this.#options.container()` returns a DO/stub with
   `getWorkspaceContainer()`. **Portable:** the host already *is* the container owner; skip this hop.
2. **`host.start(env)`** (`:198`) — launches the container with `{ PORT, MOUNT_POINT, ...env }`.
   **Portable:** `docker.containers.create({ Image, Env, HostConfig })` then `.start()`; or a k8s
   Pod spec with the `ghcr.io/cloudflare/computer-computerd-linux-x64` image.
3. **`host.interceptOutboundHttp(egressHost, workspace)`** (`:199`) — rewrites container DNS so
   `computer.internal` resolves back to the DO. **Portable:** unnecessary — we just POST
   `/connect {"url":"http://<our-host>:<port>"}` so `computerd` dials a real address. No DNS
   interception, no `Fetcher`, no `WorkspaceProxy`.
4. **`host.fetchPort(port, "http://container/connect", ...)`** (`:449-456`) — POST `/connect` into
   the container. **Portable:** plain `fetch("http://<container-ip>:8080/connect", {...})` or port-
   map the container's 8080.
5. **`probeComputerdHealth` / readiness** (`:206,400-423`) — poll `computerd`'s `/health`.
   **Portable:** identical — `fetch("http://<container-ip>:8080/health")` until ok.
6. **Accept the `/ws` reverse dial** (`handleFetch`, `:288-312`, `WebSocketPair`). **Portable:** a
   `ws.WebSocketServer` whose `connection` handler resolves the pending-connect promise and wraps
   the socket with `newWebSocketRpcSession(ws)` (the host becomes the *client* of the served stub).
7. **`monitor()` / `destroy()` / `restart()`** (`container-host.ts:129-144`, `container-lifecycle.ts`).
   **Portable:** `docker.containers.kill/remove`; k8s Pod `delete`; restart = remove + create.

Net: for **start / dial-back / exec / stream / stop**, the plain-container mapping is direct and
each CF call is a 1:1 Docker/k8s call. The only CF call with **no** portable equivalent is
`interceptOutboundHttp` — and we don't need it, because `computerd`'s dial target is a parameter,
not a DNS hack.

---

## 6. Reusability verdict per component

| Component | Verdict | One-line reason | Blockers |
|---|---|---|---|
| **`computerd`** (in-container daemon) | **REUSABLE-AS-IS** | Pure Node SEA; `/connect` takes any WS URL; no `cloudflare:` imports | Needs `/dev/fuse` (or `FUSE_MOUNT=shim`); x64-only SEA today |
| **`dofs`** (SQLite VFS + sync) | **REUSABLE-AS-IS** | Storage seam is a 3-method interface; `node:sqlite` impl ships | `:memory:` default → point at a file path; `private:true` npm pkg (vendor or fork-publish) |
| **`rpc`** (capnweb client+server) | **REUSABLE-AS-IS** | Transport-agnostic; works over node `ws`; no `cloudflare:` | `private:true` npm pkg (vendor) |
| **`Workspace` facade** (`packages/computer`) | **REUSABLE-WITH-MINOR-PORT** | `workspace.ts` is storage-agnostic, but package `index.ts` re-exports `proxy.ts` → `cloudflare:workers` import fails in Node | Vendor `workspace.ts`+non-CF deps, **or** skip it and drive dofs+rpc directly |
| **`CloudflareContainerBackend` + `container-host.ts` + `container-lifecycle.ts`** | **NEEDS-REPLACE (not "port" — replace)** | Deeply coupled to `ctx.container`, `WebSocketPair`, `cloudflare:workers` RpcTarget, DO namespace refs | Rewrite ~250 LOC as a `PortableContainerBackend` over Docker/k8s (§7) |

---

## 7. Minimal portable host (Node service on k8s)

The smallest host that delivers "container-per-session + durable VFS + one-shot streaming exec +
reverse-tunnel" reuses the three foundation packages verbatim and adds one small backend.

**Architecture per session:**

- Host holds an **authoritative** `Database` over a file-backed SQLite on a PVC
  (`/data/<session>.db`), via a one-line edit of `SQLiteTestStorage`.
- Host starts a **container** from `ghcr.io/cloudflare/computer-computerd-linux-x64:0.1.0-alpha.1`
  (the SEA image the examples already use — `examples/container/Dockerfile:1-2`).
- Host runs a **`ws.WebSocketServer`**; on `connection` it wraps the socket as a `WorkspaceRPC`
  client stub (`newWebSocketRpcSession(ws)`).
- Host **POSTs `{"url":"http://<host-svc>:<port>"}` to `computerd:8080/connect`**; `computerd`
  dials back and serves its `sync`+`shell` over it.
- Host drives **`tick(db, stub.sync)`** on a 250ms interval (same loop `computerd` uses internally,
  `packages/computerd/src/fuse/vfs.ts:127-143`) to keep the container's mirror in sync.
- Host exposes **exec** via `stub.shell.exec(command, { cwd, env, stdin })` → `ReadableStream<ExecEvent>`;
  **fs** via `WorkspaceFilesystem` over the host `Database`; **streaming** is capnweb backpressure
  end-to-end (kernel pipe → child blocks on write, `runner.ts:7-12`).

**Two implementation styles, pick by appetite:**

- **Style A — drive `dofs`+`rpc` directly (~300–500 LOC).** No `Workspace` facade. The host owns a
  `Map<sessionId, { db, stub, container }>`. Lift verbatim: `@cloudflare/dofs` (`Database`,
  `WorkspaceFilesystem`, `initializeSchema`, sync helpers), `@cloudflare/computer-rpc`
  (`createWorkspaceClient`/`newWebSocketRpcSession`, `pullOnce`/`pushOnce`/`tick` from `driver`).
  Write: a `FileSQLiteStorage` (~20 LOC), a WS accept handler (~30 LOC), a Docker/k8s lifecycle
  adapter (~150 LOC), the `/connect` POST + sync loop (~50 LOC), and your HTTP/REST surface.

- **Style B — reuse the `Workspace` facade too (~600–900 LOC).** Vendor `workspace.ts`,
  `shell.ts`, `stub.ts`, `runtime/*` (minus `bridge.ts`), `mounts/*`, `git/*`, and write one
  `PortableContainerBackend implements WorkspaceBackend` (~250 LOC) replacing
  `CloudflareContainerBackend`. You then get `Workspace.fs`, `Workspace.runtime.exec` (with
  automatic pre-exec push / post-exec pull bracketing), `Workspace.push/pull`, retry scheduling,
  and the shell router for free. Cost: carry the vendored sources and keep the `cloudflare:workers`
  re-exports out of the import graph.

**Files you'd lift vs. rewrite:**

| Lift verbatim (copy or npm-dep) | Rewrite (~LOC) |
|---|---|
| `packages/dofs/src/**` (esp. `storage.ts`, `types.ts`, `testing.ts`, `fs/*`, `sync/*`, `schema/*`) | `FileSQLiteStorage` (~20) |
| `packages/rpc/src/**` (`server.ts`, `client.ts`, `interface.ts`, `sync-driver.ts`) | WS server + `/connect` driver (~80) |
| `packages/computerd` **binary** (`ghcr.io/cloudflare/computer-computerd-linux-x64`) | Docker/k8s lifecycle adapter (~150) |
| (Style B only) `packages/computer/src/{workspace,shell,stub,runtime,mounts,git}.ts` | `PortableContainerBackend` (~250) |

The hard, novelty-dense parts — a content-addressed, chunked, coalesced, cursor-resumable SQLite
VFS with bidirectional sync and a real FUSE mount — are **all** in the "lift verbatim" column. The
"rewrite" column is mundane orchestration.

---

## 8. License & provenance

- **License: MIT**, `LICENSE:1` — *"MIT License Copyright (c) 2026 Cloudflare, Inc."* Permissive:
  copy, modify, sublicense, sell, with the copyright notice preserved. Suitable to lift into our
  project. (`README.md:125`, *"License — MIT. See LICENSE."*)
- **No `NOTICE` file** in the repo root; only `LICENSE`, `CONTRIBUTING.md`, `README.md`.
- **No CLA / contributor-license text** in `CONTRIBUTING.md` (grep for `CLA|contributor license|
  corporate|individual` returns only the unrelated *"Node 22 or newer"* engine note, `:14`).
  `.github/` holds no CLA workflow. Standard MIT; keep the Cloudflare copyright header on copied
  files.
- **Provenance caveat:** the npm packages are marked **PREVIEW / `private: true`**
  (`packages/dofs/package.json`, `packages/computer/package.json` `publishConfig.tag: "unreleased"`;
  `computer-computerd-linux-x64/README.md`: *"PREVIEW ONLY … NOT suitable for production use"*
  and *"linux-x64"* only). Plan to **vendor** (fork into our repo under MIT attribution) rather
  than depend on a published tag, and note the x64-only SEA if you need arm64.

---

## 9. Honest bottom line

**Reuse is worth it, scoped correctly.** The genuinely hard, non-obvious engineering in `computer`
— the dofs SQLite VFS (content-addressed blobs, chunk manifests, rev/path cursors, coalescing,
loopback-suppression, read-only mount enforcement), the capnweb streaming RPC with end-to-end
backpressure, and the in-container FUSE mount + process supervisor — is **entirely free of
Cloudflare runtime coupling** and runs on plain Node 22 + `node:sqlite` + `ws` + `fuse-native`.
The Cloudflare coupling is confined to ~3 files (`cloudflare-container.ts`,
`container-host.ts`, `container-lifecycle.ts`) that implement the Containers-for-Workers lifecycle
and the `computer.internal` DNS-interception trick — and that trick is **redundant** off-Cloudflare,
because `computerd`'s reverse-dial target is just a URL we supply via `/connect`. So the real work
to go off-CF is a ~250–500 LOC portable host + a `PortableContainerBackend` over the Docker/k8s
API, not a reimplementation of the VFS or sync protocol. The `open-terminal-k8s-proxy` design
(container-per-session + durable VFS + one-shot streaming exec + reverse-tunnel) is essentially
`computer`'s design — but `computer` ships the durable VFS and the FUSE mount already built and
battle-tested, which is the part most worth not rewriting. **Recommendation: reuse `dofs` + `rpc`
- `computerd` verbatim (vendored under MIT), reuse the `Workspace` facade in Style B if you want
push/pull bracketing for free, and write only the thin container backend + host service. Reimplement
from scratch only if FUSE-mounting the VFS inside the container is not a requirement (then dofs's
value shrinks to "a SQLite fs library", and a purpose-built store may be simpler).

---

## Component portability table

| Component | Verdict | Blockers | Portable equivalent |
|---|---|---|---|
| `computerd` | REUSABLE-AS-IS | `/dev/fuse` (or `FUSE_MOUNT=shim`); x64 SEA | run the `ghcr.io/cloudflare/computer-computerd-linux-x64` image; POST `/connect {url}` |
| `dofs` | REUSABLE-AS-IS | `:memory:` default; `private` npm pkg | `node:sqlite` file on a PVC via `SQLiteTestStorage` subclass |
| `rpc` (capnweb) | REUSABLE-AS-IS | `private` npm pkg | `ws.WebSocketServer` + `newWebSocketRpcSession` |
| `Workspace` facade | REUSABLE-WITH-MINOR-PORT | `index.ts` re-exports `proxy.ts` → `cloudflare:workers` | vendor `workspace.ts`; or bypass and drive dofs+rpc directly |
| `CloudflareContainerBackend` | NEEDS-REPLACE | `ctx.container`, `WebSocketPair`, `cloudflare:workers` RpcTarget, DO namespace refs | `PortableContainerBackend` over Docker SDK / CRI / k8s API (~250 LOC) |
| `container-host.ts` + `container-lifecycle.ts` | NEEDS-REPLACE | `DurableObjectState`, `ctx.waitUntil` monitor | plain Node lifecycle + `setInterval` monitor |

## Recommendation

**Reuse off-Cloudflare, scoped to the foundation.** Vendor `dofs` + `rpc` + the `computerd` binary
( MIT, attribution preserved ); write a ~250–500 LOC Node host on k8s that starts the container,
accepts `computerd`'s reverse-WS dial, drives the SQLite-on-volume VFS, and exposes exec/fs/streaming
— plus a ~250 LOC `PortableContainerBackend` if you want the `Workspace` facade's sync bracketing.
Reimplement-from-scratch is justified **only** if the in-container FUSE mount is not needed;
otherwise `computer` already delivers the hardest parts (VFS, sync, FUSE, streaming exec) for free.
