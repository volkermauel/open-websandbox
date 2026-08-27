# Coverage hardening: upgrade lane, shared×S3 matrix, PVC race, coverage report, terminals units

## Proposal

Close the five test/CI gaps identified in the post-#142 coverage analysis (issue #144).
No production-code changes; no spec deltas — this change is tests + CI + a values
profile only. The node-drain lane from the same analysis is deferred pending a
node-topology investigation and is out of scope here.

### What changes

1. **`e2e-upgrade.yml`** (new workflow, `schedule: weekly` + `workflow_dispatch`):
   KIND (runc) + chart installed at runtime tag `e2e-a`, same image retagged
   `e2e-b` loaded on the node, then `E2E_UPGRADE=1 pytest tests/e2e/test_upgrade_rollback.py`
   (PVC survives `helm upgrade --set imageTag=e2e-b`; `helm rollback` reverts the tag).
2. **`e2e.yml` `e2e-pvc` matrix** gains `pvc-s3-shared` (shared-subpath hot tier ×
   MinIO cold tier) with the new **`values-kind-pvc-shared-s3.yaml`**; the hybrid
   suite's PVC/chat-dir helpers become mode-aware.
3. **`kube_live.rs`**: concurrent + idempotent `ensure_workspace_pvc` race tests
   against the real API (AlreadyExists tolerated, one PVC, shared-mode
   missing-PVC error).
4. **`rust.yml`**: report-only `coverage` job (`cargo-llvm-cov --workspace`, lcov
   artifact upload, `continue-on-error`).
5. **`terminals.rs` unit tests**: scrollback ring evicts oldest beyond cap,
   `cap == 0` disables capture, `flush_scrollbacks` no-ops (and creates no dir)
   when scrollback is disabled.
6. **Bug fix in the hybrid test**: `_chat_dir` hashed only the session; the broker
   subPath is `sha256(user/session)[:12]`, so the purge assert inspected a
   nonexistent dir and passed vacuously.

## Rationale

- Items 1–2 execute currently-dead or missing coverage paths (upgrade mechanics,
  the second PVC hot tier under hybrid tiering).
- Item 3 locks the create-if-missing concurrency contract two concurrent resolves
  rely on; unit tests only cover the fake store.
- Item 4 makes "high coverage" measurable instead of guessed.
- Item 5 pins the #129 invariants that bit #142 (scrollback dir recreation).

## Impact / risks

- New CI jobs add ~15 min weekly (scheduled) + one extra PR lane (~13 min, only on
  the `e2e` workflow paths) + ~5 min report-only coverage per rust change.
- The vacuous-assert fix makes the purge proof REAL — the purge is expected to
  pass (manually verified during #142 debugging).

## Tasks

- [ ] terminals unit tests (ring cap, disabled no-op)
- [ ] kube_live ensure_workspace_pvc race tests
- [ ] mode-aware hybrid helpers + fixed chat-dir hash + values-kind-pvc-shared-s3.yaml
- [ ] e2e.yml pvc-s3-shared matrix arm
- [ ] e2e-upgrade.yml scheduled workflow
- [ ] rust.yml coverage job
- [ ] local verification: cargo suite, local KIND shared×S3 run, local KIND upgrade run
- [ ] docs: operations.md testing section (lanes + coverage), CHANGELOG
