# Changelog

All notable changes to **open-websandbox** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The published-image registry owner is not yet finalized; images are referenced
> as `ghcr.io/$OWNER/open-websandbox-{broker,runtime,router}` (see
> [Quickstart](docs/quickstart.md)). Version comparison links will be added
> once the canonical repository URL is fixed.

## [Unreleased]

### Changed

- **Control plane rewritten in Rust (Axum).** The Python/FastAPI broker + runtime are
  replaced by the Rust workspace under `rust/{shared,broker,runtime}`. Build & test is now
  `cargo fmt` / `cargo clippy --all-targets` / `cargo test --workspace` (real-filesystem +
  PTY integration tests); the Python `tests/unit` suite is removed. The broker ships as a
  single ~40 MiB distroless image; native tar+zstd snapshot/restore (#94). The vendored
  `kubernetes-sigs/agent-sandbox` controller + CRDs are preserved byte-for-byte, and the
  Go sandbox-router self-build is unchanged — see [issue #18](https://github.com/volkermauel/open-websandbox/issues/18).

### Security

- **Per-session broker<->runtime API key (#4).** Each sandbox pod now gets its OWN
  broker<->runtime key, replacing the single shared `RUNTIME_API_KEY` (hard cutover,
  no backward compatibility). The broker mints a fresh high-entropy key per sandbox,
  persists it to a per-session Secret `owui-runtime-key-<sandbox>`, injects it into the
  pod as a projected Secret volume (`/etc/runtime-key/api-key`), reads it back per hop
  via a stateless Secret get (the broker stays stateless — no in-memory/leader state),
  rotates it on resume, and reaps it with the sandbox. Delivery = projected per-session
  Secret volume via a broker-created direct `Sandbox` (the runtime NetworkPolicy denies
  API egress and `automountServiceAccountToken: false`, so the runtime reads the key
  from the mounted file — no API/RBAC/NetworkPolicy change); ephemeral moves off the
  warm-pool `SandboxClaim` path onto a direct per-session `Sandbox` (the controller's
  warm-pod reuse cannot project a per-pod Secret), so the warm pool is disabled by
  default (`warmPool.replicas: 0`). The vendored `kubernetes-sigs/agent-sandbox`
  controller + CRDs are byte-for-byte preserved.

## [0.1.0] - 2026-08-07

First usable release of **open-websandbox**, the multi-tenant Kubernetes sandbox
runtime that backs Open WebUI's "Open Terminal" feature. No proprietary runtime
dependencies; the control plane rests on the upstream
[`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
controller, pinned at **v0.5.3** (manifest vendored and SHA256-recorded under
[`open-websandbox-platform/upstream/`](open-websandbox-platform/upstream/)).

### Added — isolation

- **gVisor-isolated per-user sandboxes.** One `runsc` sandbox per active chat:
  filtered-syscall userspace kernel, default-deny networking, admission policy,
  `/dev/kvm` not exposed. Four isolation layers (process / network / host /
  node) per the [isolation-layers model](docs/architecture.md#isolation-layers).

### Added — control plane

- **Session-affine broker** (Python/FastAPI, [`open-websandbox-platform/broker`](open-websandbox-platform/broker)).
  Authenticates Open WebUI, get-or-creates a `SandboxClaim` keyed by
  `X-User-Id` / `X-Session-Id`, and reverse-proxies to the in-sandbox runtime
  through the Go **sandbox-router** (Pod-IP cache fast path). Two workspace
  modes, deploy-fixed via `BROKER_DEFAULT_PROFILE` / `BROKER_PERSISTENT_MODE`:
  - *Ephemeral* — claim binds a warm-pool sandbox; `/workspace` is an `emptyDir`
    destroyed when the claim is reaped.
  - *Persistent* (default) — `/workspace` lives on a **per-user RWX PVC**
    (`workspace-p-<hash>`); survives pod/image rollouts, node drain, and park.
    Alternative `shared-subpath` mode uses one shared PVC with per-session dirs.
- **Idle lifecycle: suspend → park → reap.** The broker's stateless, idempotent
  reaper **parks** persistent sandboxes after `BROKER_PARK_IDLE_SECONDS`
  (**120 s** default; pod `Suspended`, PVC kept, ~1–6 s cold resume) and
  **reaps** them after `BROKER_REAP_SECONDS` (7 d; claim + PVC deleted).
  Ephemeral claims return to the warm pool after `BROKER_IDLE_TTL_SECONDS`
  (**120 s**). No permanent pod per user.
- **Staging → chat workspace migration.** Files a user uploads before a chat
  begins land in a per-user *staging* sandbox; on first real chat the broker
  migrates the staging `/workspace` into the new chat sandbox and wipes staging
  (best-effort, leak-safe even if the move fails).
- **Warm pool.** `code-standard-warmpool` pre-warms `N` ready sandboxes from
  `code-standard-v1` so an ephemeral claim binds instantly instead of paying
  cold-start.

### Added — API surface

Exposed by the in-sandbox runtime ([`open-websandbox-platform/runtime`](open-websandbox-platform/runtime))
and reverse-proxied by the broker:

- `POST /execute` — sandboxed command; capped at `MAX_TIMEOUT` (**600 s**),
  `MAX_OUTPUT_BYTES` (1 MiB), `MAX_PROCS` (256, `RLIMIT_NPROC`).
- `GET | POST | PUT | DELETE /files/*` — read/write/list/delete under
  `/workspace` with a path-traversal guard.
- `GET /ports` — ports opened inside the sandbox.
- `WS /api/terminals/{id}` — interactive PTY (binary stdin/stdout).
- `GET /api/config` — connection-test gate
  (`{"features":{"terminal":true,"notebooks":false,"desktop":false}}`).

### Added — packaging & operations

- **Helm chart** ([`open-websandbox-platform/chart`](open-websandbox-platform/chart)):
  renders broker/router Deployments + ServiceAccounts/RBAC, the
  `code-standard-v1` SandboxTemplate, `code-standard-warmpool` SandboxWarmPool,
  shared PVC, and runtime-namespace ResourceQuota/LimitRange/NetworkPolicy.
  Prod-override guidance in [`docs/deploy.md`](docs/deploy.md).
- **Operational guardrails**: 20-pod / 50-PVC ResourceQuota, `LimitRange`,
  default-deny NetworkPolicy (DNS + HTTPS egress only; RFC1918/link-local
  blocked), gVisor `RuntimeClass` with node selector + taint toleration.
- **Single-glance status**:
  [`open-websandbox-platform/scripts/sandbox-status.sh`](open-websandbox-platform/scripts/sandbox-status.sh).

### Quality

- **Rust control-plane test suite** — `cargo test --workspace` (real-filesystem + PTY
  integration tests), enforced in `rust.yml`; supersedes the original Python `pytest` suite
  removed by the Rust rewrite (see [Unreleased]).
- **gVisor KIND end-to-end** suite under `tests/e2e/` (real `runsc` cluster in
  CI; `runc` fallback for local dev).

### Known limitations

- Not battle-tested: no load/soak/chaos suite, no stateful upgrade/rollback e2e.
- Multi-tenant isolation has **no negative (cross-user) tests yet** — see
  [`docs/release-readiness.md`](docs/release-readiness.md).
- Published-image registry owner not finalized (`ghcr.io/$OWNER/...`).
