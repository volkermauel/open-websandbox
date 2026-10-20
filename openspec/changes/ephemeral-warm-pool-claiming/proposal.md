# Proposal: ephemeral warm-pool claiming (SandboxClaim wiring)

## Why

The `SandboxWarmPool` CR is deployed but decorative: the broker has zero
`SandboxClaim` code (grep: no claim in `rust/`), so even a scaled pool is
never used. New ephemeral chats always pay a cold Sandbox create + pod boot
(~2s post pod-direct fix). Upstream v0.5.6 binds a claim to a warm Sandbox
in place (ownership transfers, pool replenishes) — instant bind.

Two structural blockers found while researching:

1. **Boot guard**: the runtime refuses to boot without a key file
   (`runtime/src/main.rs` `key_store.validate()`). Pool pods are built from
   the bare template — no per-session Secret — so every pool pod would
   CrashLoop today.
2. **Per-session keys can't reach warm pods**: upstream adopts the running
   pod in place; `spec.env` injection forces a cold start ("environment
   variables … cannot be injected into an already-running warm pool Pod",
   upstream api.md); volumes cannot be added to a running pod.

User-approved design: **pool-scoped key for ephemeral only**, baked into a
**dedicated pool template**. Per-session keys (#147, rotate-on-resume) stay
for persistent. Ephemeral workspaces are disposable emptyDir with ingress
restricted to `agent-sandbox-system` — the hop auth downgrade is accepted
and documented.

## What Changes

- **Dedicated pool template** `code-standard-ephemeral-v1` (chart): copy of
  `code-standard-v1` + `runtime-key` Secret volume from the new pool key.
  The persistent template is untouched — no dead volumes, no injection-skip.
- **Pool key Secret** `owui-pool-key` (chart): value `broker.poolRuntimeKey`
  (set `openssl rand -hex 32` in prod, same pattern as `sharedSecret`).
  `warmPool.templateName` → the pool template; `warmPool.replicas` default 1.
- **Broker claim path** (`Profile::Ephemeral` in `resolve_sandbox`):
  get-or-create `SandboxClaim{name: owui-e-<hex12>, warmPoolRef: <pool>}`
  with broker labels; poll `claim.status.sandbox.name` (controller binds a
  warm Sandbox or creates one); patch the bound Sandbox with the standard
  broker labels + `owui.io/claim` annotation; then the existing pod-direct
  `wait_for_ready` resolves it. `BROKER_WARMPOOL` (new config field, from
  `broker.warmPool`) names the pool; empty disables claiming (claim-less
  fallback = today's direct-Sandbox path).
- **Key fallback**: `read_runtime_key(sandbox)` → per-session Secret, and on
  NotFound + sandbox labeled `owui.io/auth=pool-key` → pool key. Uniform for
  terminal/proxy/S3 call sites. Fail-closed when neither exists.
- **Reaper**: ephemeral reap deletes the **claim** (annotation lookup);
  cascade removes Sandbox+pod, pool replenishes. Persistent untouched.
- Persistent path: byte-for-byte unchanged behavior.

Non-goals: persistent warm pooling (structurally impossible with per-chat
PVCs), pool autosizing, `shutdownPolicy` lifecycle (reaper owns timing),
v1alpha1 claims (deprecated).

MCP/frontend: no API surface change (internal resolve path + chart). MCP
tooling unaffected.

## Impact

- `rust/shared/src/crd.rs` — `SandboxClaim` typed wrapper (v1beta1)
- `rust/shared/src/config.rs` — `warm_pool` field + `BROKER_WARMPOOL`
- `rust/broker/src/store.rs` — `create_claim` / `get_claim` / `delete_claim`
  / `label_sandbox`; stub doubles
- `rust/broker/src/resolve.rs` — ephemeral claim branch
- `rust/broker/src/reaper.rs` — claim-aware ephemeral reap
- `open-websandbox-platform/chart/templates/{sandboxtemplate-pool.yaml,warmpool.yaml,pool-key-secret.yaml}`
  + `values.yaml` — pool template, key, replicas
- `rust/broker/tests/`, `rust/shared/src/config.rs` tests
- `docs/operations.md`, `docs/deploy.md`, `docs/architecture.md` claim note
- specs: `broker/warm-pool-claiming/spec.md` (new)
