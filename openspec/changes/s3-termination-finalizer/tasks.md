# Tasks: S3 termination finalizer

## 1. Config + chart + manifests

- [ ] 1.1 `shared/config.rs`: `BROKER_S3_TERMINATION_FINALIZER` (bool, default
      true; effective only in s3-record mode) + chart value
      `broker.s3.terminationFinalizer`; `values.schema.json` entry
- [ ] 1.2 Chart s3 pool template: `terminationGracePeriodSeconds: 45`,
      `preStop: ["sleep","30"]`; keep `deploy/base` byte-for-byte in sync
- [ ] 1.3 Finalizer name constant in `shared/crd.rs`
      (`owui.dev/s3-snapshot`)

## 2. Broker

- [ ] 2.1 Register the finalizer on managed Sandboxes (claim bind + cold
      create) in s3-record mode; never in per-user-pvc mode
- [ ] 2.2 Deletion branch in the sandbox watch: on `deletionTimestamp`,
      dedupe against the chat's current snapshot generation, offload via
      `S3Offload`, remove finalizer on success; retry with backoff on S3
      failure; alert-grade log on blocked deletion
- [ ] 2.3 Unit tests: dedupe short-circuit, out-of-band delete archives,
      offload failure holds the finalizer, mode gating (per-user-pvc
      unaffected)

## 3. e2e + docs

- [ ] 3.1 e2e (s3 lane): `kubectl delete sandbox` mid-session → Sandbox
      Terminating until `latest.zst` updated, then gone; content survives
      into a fresh resolve
- [ ] 3.2 e2e (s3 lane): drain-style direct pod delete → belt path archives
      (assert S3 object generation advanced)
- [ ] 3.3 `docs/operations.md`: deletion ordering guarantees, Terminating
      expectation, alert meaning; `docs/architecture.md` lifecycle diagram
      note
