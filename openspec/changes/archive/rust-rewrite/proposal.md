# Rust rewrite — broker + runtime (bounded end-to-end memory safety)

Issue: #18. Rewrites the **broker** and **runtime** control-plane components from
Python/FastAPI to **Rust (axum + tokio)**, moving today's *hand-written + unverifiable*
memory-safety guards (`_safe_path` confinement, `hmac.compare_digest`, process-group
tree-kills, output truncation, `RLIMIT_NPROC`) into the type system over a single audited
dependency tree. **Big-bang cutover** (no Python shim/coexistence).

## Scope — bounded "end-to-end" (D1)

Research (three read-only subagent maps of broker/runtime/tests/chart/deps) confirms:

- **IN scope:** broker (Python→Rust), runtime (Python→Rust).
- **OUT — router (D3 of #18):** stays Go. Already memory-safe; rewriting buys nothing.
- **OUT — upstream controller:** `registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.5.3`
  is an **opaque prebuilt Go image we pull at deploy** (vendored byte-for-byte; we do not
  compile its source). It cannot be rewritten here.
- **Residual Go bookends** (router + controller) traverse the data path — documented as
  accepted residual risk. "End-to-end memory safety" therefore means **our authored control
  plane** (broker + runtime); a future fork+own-build of the router/controller is a separate
  effort.

## Why

Python carries no compile-time guarantees; the safety the platform relies on today is
correct but only checkable at runtime, sitting on a large CPython/stdlib surface. Rust moves
those guarantees into the type system (`#![forbid(unsafe_code)]` in our crates), enables
fuzzing/Miri on the security-critical paths, and yields a far smaller audited dependency
tree + drastically smaller/faster broker image.

## Decisions (D1–D16, locked in #18 comments)

| ID | Decision | Resolution |
|----|----------|------------|
| D1 | scope of "end-to-end" | **bounded**: broker + runtime only; router/controller stay Go (documented) |
| D2 | workspace + layout | single Cargo workspace at repo-root `rust/` → crates `broker`, `runtime`, `shared` |
| D3 | k8s CRD typing | hand-written type-safe structs (`kube::CustomResource` derive) for `agents.x-k8s.io/v1beta1` Sandbox, `extensions.agents.x-k8s.io/v1beta1` SandboxTemplate (+ Claim/WarmPool/Secret/PVC/Lease) |
| D4 | S3 client | `aws-sdk-s3` (official, feature-gated; MinIO path-style + per-object `delete_object` [#56] + multipart + expiry + SSE-S3) |
| D5 | PTY / WS terminal | `tokio-tungstenite` + `portable-pty`; 1:1 binary/text frames, close codes 4001/4004, `$SHELL` `start_new_session`, `TERM=xterm-256color`, 24×80, cap `MAX_TERMINAL_SESSIONS=8`→429 |
| D6 | snapshot/restore tarball | **native `tar`+`zstd` binaries** (no Rust tar/zstd crates — don't reinvent the wheel) |
| D7 | exec / process control | `tokio::process` + `nix` (setsid/killpg/setrlimit); timeout = immediate SIGKILL of the process group (match Python); `exit_code=124` on timeout; HTTP 200 on non-zero; `MAX_OUTPUT_BYTES` cap |
| D8 | memory-safety rigor | `#![forbid(unsafe_code)]` in our crates; vetted deps via `cargo-deny`/`cargo-audit` + `clippy -D warnings`; `cargo-fuzz` + Miri on path-confinement + auth |
| D9 | telemetry | `opentelemetry-rust` + `prometheus` crate; soft-OTel (no-op when `OTEL_EXPORTER_OTLP_ENDPOINT` unset); identical metric names (`open_websandbox_broker_*`/`open_websandbox_runtime_*`) + label cardinality → Grafana dashboard (#49) unchanged |
| D10 | OpenAPI | **`utoipa`-generated** from Rust types/endpoints; `broker/openapi_spec.py` **deleted**; frozen-snapshot test guards the OWUI-facing shape |
| D11 | HTTP parity | **strict 1:1** — paths/methods/bodies/status/streaming/error-bodies/WS-close-codes; deviations are bugs |
| D12 | config/env | **drop-in** — same env-var names/values; chart env blocks unchanged |
| D13 | images | broker → multi-stage cargo → `gcr.io/distroless/cc-debian12` (~40 MiB single executable; ~26 MiB stripped [accepted #83 — debug symbols kept for backtraces]); runtime → **unchanged debian/python base** (keeps `tar`+`zstd` [D6] + tenant data-science toolchain [D16]); Rust server binary replaces `uvicorn`; same image names/tags |
| D14 | testing | Rust unit tests port ~314 Python unit tests (esp. 17 `test_safe_path`, `test_s3_tiered`, `test_leader`, `test_reaper`, `test_runtime_auth`); **keep the 13 black-box e2e tests in Python** (language-agnostic; run unchanged against Rust images × gVisor/runc + S3 + env); CI adds `cargo test`/`clippy`/`fmt`/`deny`/`audit` (+ Miri/fuzz) |
| D15 | phasing | PR-A workspace+shared+CI → PR-B runtime → PR-C broker → PR-D chart swap + full e2e (**cutover**) → PR-E Python removal |
| D16 | Python removal | delete broker/runtime `.py` + `openapi_spec.py` + `entrypoint.sh` + `requirements*` + Python Dockerfiles; **keep** `tests/e2e/*` (contract driver) + update `mkdocs` |

## Contract preservation (the big-bang invariant)

Because the cutover is big-bang, the **Open Web UI client must not break**:

- HTTP API 1:1 (D11), env contract drop-in (D12), same image names/tags → the chart is an
  **image swap only** (`broker.yaml` + `sandboxtemplate.yaml`); all other templates
  (RBAC, NetworkPolicy, ResourceQuota, PVC, WarmPool, monitoring, CRDs, upstream controller,
  router) are language-agnostic and stay byte-for-byte.
- The 13 black-box e2e tests are the cross-implementation gate (D14): they drove the Python
  impl, they drive the Rust impl unchanged.

## Risks (top 5)

1. **PTY/terminal over WS (D5)** — hardest port; binary/text frame protocol + gVisor PTY.
2. **k8s CRD typing (D3)** — hand-written structs must track upstream v1beta1 exactly.
3. **leader election parity** — `kube-runtime::leader_election` must match the current
   Lease semantics (same name/namespace/renew/deadline).
4. **path confinement** — the Rust `_safe_path` equivalent must pass all 17 `test_safe_path`
   cases verbatim (symlink/`..`/escape).
5. **S3-tiered parity** — offload/restore/retention + MinIO `delete_object` quirk + D7
   ordering (#56) all carry over.

## Phasing — see `tasks.md`. Spec deltas — see `specs/open-websandbox-platform/spec.md`
