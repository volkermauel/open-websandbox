# open-webui-terminal2cloudflarecompute

A portable, high-density command-execution runtime for Open WebUI's **Open Terminal**
integration, derived from Cloudflare's open-source **`computer`** project — but with
**zero Cloudflare-proprietary dependencies**.

## What this project is

A single Node service (the **gateway**) that re-exposes the **open-terminal REST/OpenAPI
contract** that Open WebUI already speaks (`/execute`, `/files/*`, `/health`, `/api/config`,
`/system`, `/info`, `/openapi.json`), and delegates execution to a single shared, long-lived
**`computerd`** sidecar (Cloudflare's in-container FUSE + exec daemon), reused verbatim.

Per-session isolation is provided by an **nsjail jail** that wraps each `exec`, rather than by
spawning a pod/container per session — maximising density.

## What we reuse from `github.com/cloudflare/computer` (MIT, vendored)

| Component | Role | Portable off-CF? |
|---|---|---|
| `dofs` | SQLite-backed virtual filesystem + content-addressed sync | **Yes** (node:sqlite) |
| `rpc` / capnweb | transport-agnostic streaming RPC over WebSocket | **Yes** (node `ws`) |
| `computerd` | in-container daemon: FUSE-mounts the VFS, runs `/bin/sh -c` exec | **Yes** (plain Linux + /dev/fuse) |

We do **not** use `packages/computer`'s host facade or `CloudflareContainerBackend` — those are
coupled to `ctx.container` / `ctx.storage` / Durable Objects. See `research/04-*.md`.

## Threat model & isolation stance

- **Must:** per-user isolation (distinct VFS subtrees + distinct jail uid/netns + no cross-user
  file visibility).
- **Ideal:** per-chat isolation (same mechanism, one subtree + jail per chat).
- **Accepted residual risk:** all sessions share one kernel and one `computerd` process. A
  kernel exploit or jail misconfiguration could cross sessions. Acceptable because users are
  trusted internal (Entra OIDC) accounts, not anonymous internet tenants.

## Key documents

- `openspec/changes/use-computerd-as-runtime/proposal.md` — what & why
- `openspec/changes/use-computerd-as-runtime/design.md` — architecture & decisions
- `openspec/changes/use-computerd-as-runtime/tasks.md` — phased implementation
- `research/01-cloudflare-computer.md` — computer internals
- `research/02-open-terminal.md` — the OWUI contract to reproduce
- `research/03-k8s-proxy-isolation-prior-art.md` — isolation requirements baseline
- `research/04-computer-portability-off-cf.md` — off-CF portability verdicts
