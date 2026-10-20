# Tasks: terminal-open latency

## 1. Broker pod-direct readiness

- [x] 1.1 `store.rs`: `SandboxStore::get_pod(name) -> Option<Pod>` — KubeStore
      GET (NotFound ⇒ None), StubSandboxStore backed by a pods map +
      `mark_pod_ready(name, ip)` helper
- [x] 1.2 `resolve.rs`: `wait_for_ready` checks the pod first each tick
      (`Ready=True` + `podIP` ⇒ resolved; one best-effort sandbox GET for the
      S3 annotation), Sandbox path stays as fallback
- [x] 1.3 `resolve.rs`: pod-condition digest in the timeout 503
- [x] 1.4 tests: pod-ready-before-sandbox-status short-circuit; fallback when
      pod absent; digest rendering

## 2. Chart + base manifests

- [x] 2.1 SandboxTemplate readiness probe `periodSeconds: 2 → 1` (chart
      template + `deploy/base/sandboxtemplate-code-standard.yaml`)
- [x] 2.2 `broker.parkIdleSeconds` default `120 → 900` (values.yaml, schema
      description, `deploy/base/broker.yaml`)

## 3. Docs + spec

- [x] 3.1 `docs/operations.md`: measured latency budget, tuning knobs
      (parkIdleSeconds vs idle-pod cost), gVisor/runc note
- [x] 3.2 spec delta `specs/broker/resolve/spec.md`: pod-direct readiness
      contract

## 4. Verification

- [x] 4.1 `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test --workspace`
- [x] 4.2 `helm lint` + `helm template` parity vs `deploy/base`
- [x] 4.3 e2e unaffected check (values-kind park overrides still valid)
