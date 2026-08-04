# Comparison — computerd approach vs AgentSandbox.md

> `AgentSandbox.md` (in repo root) is a "Proposed implementation specification" for an
> on-prem Kubernetes sandbox platform built on **Kubernetes SIG Agent Sandbox + gVisor + a Go
> broker**. This document compares it to the `use-computerd-as-runtime` change
> (`proposal.md`/`design.md`), so we can decide which to build (or how to combine them).

## TL;DR — the fundamental tension

The two designs are **almost diametrically opposed** on the two axes you care about most, and
**you cannot have both extremes at once**:

| | computerd approach | AgentSandbox.md |
|---|---|---|
| Density / "no pod per session" | ✅ **One shared container**, all sessions | ❌ **One gVisor pod per active session** (warm-pool sourced) |
| Isolation strength | ❌ nsjail inside one shared **privileged** container; **shared kernel** | ✅ **gVisor** (userspace kernel), hostile-tenant grade |

Your stated hard-ish constraint — *"ideally use computerd so we don't spawn a pod per session"* —
is satisfied **only** by the computerd approach. AgentSandbox.md instead spawns a pod **per active
session** (sourced from a warm pool, destroyed after use) to get gVisor isolation. **gVisor-grade
isolation is fundamentally incompatible with "no pod/container per session"** — a userspace kernel
has to run somewhere per session. So the choice is really: *how much isolation are you willing to
trade for density?*

AgentSandbox.md's warm-pool model is a genuine middle path the computerd design lacks: it avoids
the **old k8s-proxy's** sin (permanent pod + PVC **per user**, churn on every session) by pooling

+ destroying-after-use — but it is still "pod per active session."

## Side-by-side across all dimensions

| Dimension | computerd (`use-computerd-as-runtime`) | AgentSandbox.md |
|---|---|---|
| Foundation | Cloudflare `computer` (vendored `dofs`+`rpc`, image `computerd`) | Kubernetes SIG Agent Sandbox CRDs/controllers (`v0.5.3`, `v1beta1`) + gVisor |
| Isolation primitive | nsjail jail **per exec** (namespaces + cgroups + seccomp) inside **one shared container** | gVisor `runsc` **RuntimeClass** on dedicated tainted nodes, **one pod per session** |
| Pod/container per session | **None** — 1 shared `computerd` sidecar for everyone | **One per active session**, from a `SandboxWarmPool`; destroyed & replaced after use, never reused |
| Threat model | **Trusted internal users** (Entra OIDC); accepted shared-kernel residual risk | **Hostile workloads** (sandbox pod = "Hostile workload"); Kata later for mutually hostile tenants |
| Worker pod privilege | **`privileged: true`** (needs `/dev/fuse` + nsjail caps) — **violates AS invariant #7** | `privileged:false`, drop ALL caps, `runAsNonRoot`, `readOnlyRootFilesystem`, seccomp RuntimeDefault |
| Workspace durability | **Durable by default** — `dofs` SQLite on PVC (content-addressed, sync-able, snapshot-able) | **Ephemeral by default** (`emptyDir`); durable artifacts uploaded to **internal S3** before session end |
| Egress | Phase 1 **shared netns** (commands have network); per-user policy = Phase 2 | **Default-deny**, explicit allowlist via egress proxy / Cilium FQDN / gateway; blocks K8s API, node, mgmt nets |
| Edge / control plane | Single **gateway** (Node) = broker+router+runtime-host combined | Separate **gateway → broker (Go) → router → sandbox**; strict trust boundaries; router not user-reachable |
| AuthN/Z | Shared Bearer + trusted `X-User-Id`/`X-Session-Id` headers | OIDC via auth proxy; broker derives identity, **strips caller routing headers**, signed session tokens, per-op authz |
| State / recovery | Stateful (`dofs` SQLite + `sessions_meta`); re-adopts on restart | **Stateless v1** (no DB) — k8s claims/annotations are the registry; reconciles orphans on restart |
| Open WebUI integration | **Drop-in**: reproduces open-terminal REST/OpenAPI contract verbatim | Owns its **own broker API + optional MCP**; needs an OWUI adapter (OWUI speaks MCP, so feasible) |
| Operational maturity | Greenfield, ~650 LOC, no admission/observability/runbook yet | Full GitOps, ValidatingAdmissionPolicy, ResourceQuota/LimitRange, 16 invariants, 22 security tests, runbook, dashboards, alerts |
| Reused upstream | `dofs`/`rpc`/`computerd` (Cloudflare, MIT, alpha) | k8s-sigs Agent Sandbox + gVisor (CNCF/Linux, supported) |
| Concurrency target | one writer (1 replica v1); shard later | 10 active + 2 warm = 12 slots; sized via quota |

## Where AgentSandbox.md **validates** our computerd choices

Both designs independently arrive at the same control-plane shape — good sign the bones are right:
+ No permanent pod per user; isolation keyed per **session**, destroyed after use.
+ A broker/gateway that owns **auth + routing + header-stripping + quotas + lifecycle + restart recovery**.
+ Ephemeral-vs-durable workspace is a **config/profile** choice, not baked in.
+ Egress **default-deny** is the goal (AS ships it; we deferred to Phase 2).
+ Optional MCP surface; reproducible GitOps deploy with pinned digests.

## Where AgentSandbox.md is **objectively stronger**

1. **Isolation.** gVisor intercepts syscalls in userspace; nsjail in a shared privileged container
   does not. AS's §23 test list (read SA token, reach K8s API, mount host FS, run as root, escape
   `/workspace` via symlink, fork-bomb, exceed mem) **cannot pass** in our privileged-shared-kernel
   model — several are trivially possible there.
2. **Privilege posture.** Our `privileged: true` worker (for `/dev/fuse` + nsjail) is exactly what
   AS's admission policy **rejects**. A compromised worker pod in our design is a full cluster-node
   compromise.
3. **Operational rigor.** AS ships admission policy, quotas, NetworkPolicy, observability, audit,
   runbook, emergency-stop. Our design is a sketch next to it.
4. **Upstream support.** k8s-sigs Agent Sandbox + gVisor are maintained, documented, pre-1.0 but
   real. `computer` is `0.1.0-alpha.1` "PREVIEW ONLY".

## Where the computerd approach is **genuinely ahead or different-by-design**

1. **Durable per-chat workspace by default.** For an Open WebUI terminal (long-lived chats,
   iterative agent work), `dofs`-on-PVC is more useful than AS's "upload to S3 before you lose it."
   AS treats workspace persistence as an exception; we treat it as the point.
2. **Drop-in OWUI compatibility.** We reproduce the open-terminal contract so Open WebUI works
   unchanged. AS needs an OWUI-facing adapter (its broker API is its own).
3. **Max density / min cost.** One container for everyone; ~ms cold start. AS pays a gVisor pod
   per active session (cheap + warm, but not free).
4. **Content-addressed VFS** (`dofs`) enables future "snapshot / branch / reset this chat's
   filesystem" cheaply — AS has no equivalent.

## The decision matrix (pick your priority)

| You prioritise… | → Choose |
|---|---|
| **Max density, no pod per session**, trusted users only, durable workspaces | **computerd** (accept nsjail + privileged + shared kernel) |
| **Hostile-tenant isolation**, supported upstream, ops maturity, audit | **AgentSandbox.md** (accept pod-per-active-session via warm pool) |
| **Both** strong isolation *and* durable VFS *and* drop-in OWUI | **Hybrid** (see below) |

## Hybrid option C (worth real consideration)

Use **AgentSandbox.md as the platform shell** (gVisor pods, warm pool, broker, admission,
NetworkPolicy, GitOps) and make the **runtime image inside each sandbox be computerd-backed** so
you also get the durable `dofs` VFS + OWUI open-terminal contract:

```
OWUI ──▶ open-terminal-contract adapter ──▶ Agent Sandbox Broker
                                              │ SandboxClaim (warm pool)
                                              ▼
                                   gVisor sandbox pod (strong isolation)
                                     └─ runtime = computerd + dofs(SQLite on emptyDir/PVC)
                                          (FUSE VFS + exec), OR plain runtime-server + dofs
```
+ **Pros:** gVisor isolation + warm-pool density + durable VFS + drop-in OWUI. Reuses AS's whole
  ops surface.
+ **Risks to validate (Phase-0):**
  + **gVisor + FUSE**: `runsc` historically has limited/unreliable `/dev/fuse`; may force the
    `FUSE_MOUNT=shim` fallback (exec then can't write the VFS as real files). If FUSE is unusable
    under gVisor, drop FUSE and have the runtime-server drive `dofs` directly (SQLite read/write)
    with exec seeing a bind-mounted dir — keeps durability, loses the FUSE "files appear magically"
    property.
  + nsjail inside gVisor is **redundant** (gVisor already isolates) and may conflict — drop it;
    rely on gVisor + the AS pod security profile.
  + Do you still need `computerd` at all, or just `dofs`? If FUSE is out, `computerd`'s only
    remaining value is its exec runner (trivial) — you might keep only `dofs` for the VFS and use
    AS's stock runtime-server for exec. That narrows "use computer" to "use `dofs`."

## Recommendation

+ If the **threat model is genuinely "trusted internal users, prevent accidental leakage"** and
  density/no-pod-per-session is the hard constraint → **proceed with `use-computerd-as-runtime`**,
  but **borrow AS's hardening**: add a real seccomp profile, drop capabilities, run the gateway
  non-root where possible, add NetworkPolicy + ResourceQuota + admission checks + observability,
  and treat "privileged worker" as a contained, node-isolated blast radius (dedicated tainted
  node, like AS §6.2).
+ If **you want defensible, auditable, hostile-tenant isolation on supported upstream** →
  **adopt AgentSandbox.md** as the foundation; layer our durable-VFS + OWUI-contract ideas on top
  (Hybrid C), validating the gVisor/FUSE question first.
+ The single question that decides it: **are OWUI terminal users trusted (→ computerd) or
  potentially hostile (→ AgentSandbox)?** Everything else flows from that.

> Note: this comparison does not change any OpenSpec artifact yet. Once you pick a direction,
> `use-computerd-as-runtime` should either (a) absorb AS hardening as new requirements, (b) be
> superseded by an Agent-Sandbox-based change, or (c) be re-scoped to Hybrid C.
