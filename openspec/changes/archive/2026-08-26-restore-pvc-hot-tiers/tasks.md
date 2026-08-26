# Tasks

## 1. Rust broker

- [x] 1.1 `shared/config.rs`: `PersistentMode` enum (+`FromStr`/serde, fail-closed on
      unknown), config fields `persistent_mode`, `persistent_storage`,
      `persistent_storage_class`, `persistent_access_modes`, `shared_pvc_name`,
      `per_user_pvc_prefix`; boot validation `s3-tiered ⟺ s3_enabled`; unit tests.
- [x] 1.2 `broker/sandbox.rs`: `apply_persistent_volume` pure surgery + unit tests.
- [x] 1.3 `broker/store.rs`: `ensure_workspace_pvc` (trait + kube + in-memory stub) +
      tests.
- [x] 1.4 `broker/resolve.rs`: mode dispatch, PVC ensure, surgery, mode label;
      resolve tests.
- [x] 1.5 `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`

## 2. Chart + base

- [x] 2.1 `broker.yaml`: new env (STORAGE_CLASS, ACCESS_MODES, SHARED_PVC,
      PER_USER_PVC_PREFIX); sync `deploy/base/broker.yaml`.
- [x] 2.2 `shared-pvc.yaml`: render only in `shared-subpath` mode; `accessModes`
      value; delete `deploy/base/workspace-shared-pvc.yaml` (default mode no longer
      renders it).
- [x] 2.3 `values.yaml` + `values.schema.json`: new knobs, mode comment includes
      `s3-tiered`.
- [x] 2.4 `values-kind-pvc.yaml` + `values-kind-pvc-shared.yaml` (local-path RWO).
- [x] 2.5 `helm lint` + render-diff chart↔base parity.

## 3. e2e

- [x] 3.1 `conftest.py`: `require_pvc` fixture (E2E_PVC=1, lazy port-forward).
- [x] 3.2 `tests/e2e/test_pvc_persistence.py`: persistence across sandbox
      delete/re-resolve; cross-chat isolation (visibility + delete); cross-user
      isolation (shared mode); PVC naming.
- [x] 3.3 `.github/workflows/e2e.yml`: `e2e-pvc` job, matrix over both PVC modes.
- [x] 3.4 Local KIND run of both PVC lanes (R1: dedicated kubeconfig).

## 4. Docs

- [x] 4.1 `docs/deploy.md`: mode table (per-chat subPath semantics), env table
      (`profile.*` → `broker.*` fix), accessModes/storageClass guidance.
- [x] 4.2 `docs/architecture.md` + `docs/operations.md` + `AGENTS.md` gotcha line.
- [x] 4.3 `CHANGELOG.md`.
- [x] 4.4 `mkdocs build --strict` passes.
