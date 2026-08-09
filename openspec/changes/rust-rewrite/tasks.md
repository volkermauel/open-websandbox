# Tasks

## Phase 0 — OpenSpec change

- [x] `openspec/changes/rust-rewrite/`: proposal + spec + tasks encoding D1–D16.
- [ ] Commit.

## Phase 1 — PR-A: Cargo workspace + `shared` crate + Rust CI

- [ ] `rust/` Cargo workspace: `Cargo.toml` (workspace), `rust/shared`, `rust/broker`,
      `rust/runtime` members; `.gitignore` for `target/`; committed `Cargo.lock`.
- [ ] `rust/shared`: `#![forbid(unsafe_code)]`; OpenAPI request/response types via `utoipa`
      derives; hand-written `agents.x-k8s.io/v1beta1` Sandbox +
      `extensions.agents.x-k8s.io/v1beta1` SandboxTemplate (+ SandboxClaim, SandboxWarmPool)
      as `kube::CustomResource` structs; common config/env parsing (drop-in names, D12) +
      per-session-key auth helpers (constant-time compare — `subtle`).
- [ ] `.github/workflows/rust.yml`: `cargo fmt --check`, `cargo clippy -- -D warnings`,
      `cargo test`, `cargo deny check` / `cargo audit` (+ Miri job on the path-confinement +
      auth units). Runs on the workspace.
- [ ] `rust-toolchain.toml` (stable + fmt/clippy components).
- [ ] Empty `rust/broker` + `rust/runtime` crates (lib skeletons) so the workspace + CI are
      green; real code lands in PR-B/PR-C.

## Phase 2 — PR-B: `runtime` in Rust

- [ ] `rust/runtime/src`: port `runtime/server.py` — axum app; endpoints `GET /`
      (`{status,runtime}`), `GET /healthz`, `GET /readyz`, `GET /metrics`, `GET /ports`
      (auth, always empty), `POST /execute` (auth), `/files/*` (cwd/list/read/write/mkdir/
      move/delete/replace/grep/glob/upload/archive/view), `/upload`, `/download/{path}`,
      `/list/{path}`, `/exists/{path}`, `GET /snapshot` + `PUT /restore` (auth).
- [ ] Per-session-key auth: read `/etc/runtime-key/api-key` (mtime-cached reload-on-mismatch
      → rotate-on-resume, #50); `Security` extractor; fail-closed boot guard; 401/403.
- [ ] `/execute`: `tokio::process` + `nix` setsid (process-group) + `killpg(SIGKILL)` on
      timeout (exit_code=124, HTTP 200 on non-zero) + `RLIMIT_NPROC=MAX_PROCS` +
      `MAX_OUTPUT_BYTES` stream cap.
- [ ] `/snapshot`+`/restore`: spawn native `tar`+`zstd` (D6); pre-check 413
      (`>MAX_WORKSPACE_BYTES`); no leading `.` entry; compressed-size cap; rc≠0→500.
- [ ] `/api/terminals` WebSocket PTY (D5): `tokio-tungstenite` + `portable-pty`; binary =
      pty I/O, text control `{resize|auth}`; close codes 4001/4004; `$SHELL`
      `start_new_session`; `TERM=xterm-256color`; 24×80; `MAX_TERMINAL_SESSIONS=8`→429.
- [ ] `utoipa` derives → generated `/openapi.json` (D10); frozen-snapshot test.
- [ ] `/metrics` (prometheus, `open_websandbox_runtime_*`) + soft OTel (#49 parity).
- [ ] `rust/runtime/Dockerfile`: unchanged debian/python base + tenant data-science layer
      (D13/D16) + Rust server binary; entrypoint swaps `uvicorn`→the Rust binary.
- [ ] Unit tests: port `test_safe_path` (17 — **security-critical, verbatim**),
      `test_files_*`, `test_execute*`, `test_runtime_auth`, `test_snapshot_restore`,
      `test_terminal*`, `test_metrics`.

## Phase 3 — PR-C: `broker` in Rust

- [ ] `rust/broker/src`: port `broker/main.py` — axum app; HTTP surface (`/execute`,
      `/files/*`, `/ports`, `/api/terminals[/{id}]` POST/GET + WS relay, `/metrics`,
      `/readyz`, `/healthz`, `/api/config`, `/api/status`, catch-all reverse proxy).
- [ ] `kube-rs` typed clients (D3): Sandbox/SandboxTemplate/SandboxClaim/SandboxWarmPool +
      Secret/PVC/Lease CRUD + Watch; `kube-runtime::leader_election` (parity with current
      Lease name/renew/deadline).
- [ ] Session→sandbox resolution; warm pool; persistent modes (per-user-pvc /
      shared-subpath / s3-tiered); per-session-key Secret lifecycle (#50) + orphan sweep (#51).
- [ ] Reaper loop (idle TTL, double-reap avoidance) + periodic s3 sync (R1); S3-tiered
      offload-on-reap + restore-on-resume via the runtime endpoints (#52) with **upload-new-
      then-delete-old ordering (#56)**; keep-latest via per-object `delete_object` (MinIO).
- [ ] `aws-sdk-s3` (D4, feature-gated) soft-import semantics.
- [ ] `/metrics` (`open_websandbox_broker_*`, bounded path labels) + soft OTel.
- [ ] `rust/broker/Dockerfile`: multi-stage cargo → `gcr.io/distroless/cc-debian12`
      (D13); ~15–30 MB.
- [ ] Unit tests: port `test_k8s`, `test_resolve`, `test_reaper`, `test_leader`,
      `test_s3_tiered`, `test_endpoints`, `test_migrate`, `test_terminal_proxy`,
      `test_openapi_version`, `test_observability`, `test_helpers`, `test_coverage_gaps`.

## Phase 4 — PR-D: chart image swap + full e2e parity (CUTOVER)

- [ ] `chart/templates/broker.yaml` + `chart/templates/sandboxtemplate.yaml`: image refs →
      Rust-built images (same names/tags); env blocks unchanged (D12); probes/ports
      unchanged. No other template touched.
- [ ] Build the 3 images (broker, runtime, router unchanged) in CI; `kind load`.
- [ ] **Full e2e green against Rust images**: `test_smoke`×5, `test_isolation`×5
      (path-traversal, NetworkPolicy, env-no-secrets, no-service-links),
      `test_s3_tiered`×3 (MinIO offload/restore/isolation) — × gVisor/runc matrix.
- [ ] `helm lint` + `helm template` clean + default byte-identical.

## Phase 5 — PR-E: Python removal

- [ ] Delete `open-websandbox-platform/broker/{main.py,openapi_spec.py,requirements*.in,
      requirements*.txt,Dockerfile}`, `open-websandbox-platform/runtime/{server.py,
      entrypoint.sh,requirements*.txt,Dockerfile}` (D10/D16).
- [ ] Keep `tests/e2e/*` + `conftest.py` (cross-impl contract driver, D14); remove Python
      unit tests now ported to Rust (`tests/unit/*`).
- [ ] Update `mkdocs` + `docs/` Python references → Rust.
- [ ] `openspec validate`; archive the `rust-rewrite` change.
