# Tasks: ephemeral warm-pool claiming

## 1. Shared: CRD + config

- [x] 1.1 `shared/src/crd.rs`: `SandboxClaim` v1beta1 typed wrapper
      (`warmPoolRef`, optional `additionalPodMetadata`, status `sandbox{name,
      podIPs}`)
- [x] 1.2 `shared/src/config.rs`: `warm_pool: String` (env `BROKER_WARMPOOL`,
      default `code-standard-warmpool`; empty disables claiming) + tests

## 2. Broker store

- [x] 2.1 Trait + KubeStore + stub: `create_claim` (Conflict on exists),
      `get_claim`, `delete_claim`, `label_sandbox`
- [x] 2.2 `read_runtime_key` pool-key fallback: per-session Secret NotFound
      AND sandbox labeled `owui.io/auth=pool-key` → `owui-pool-key` Secret

## 3. Broker resolve + reaper

- [x] 3.1 `resolve.rs`: ephemeral branch — get-or-create claim (labels +
      `broker-profile=ephemeral`), poll `status.sandbox.name`, patch bound
      Sandbox labels + `owui.io/claim` annotation, resolve via pod-direct
      `wait_for_ready`; claim-less fallback when `warm_pool` empty
- [x] 3.2 `reaper.rs`: ephemeral Reap with `owui.io/claim` annotation
      deletes the claim (fallback: delete sandbox); annotate + log claim path

## 4. Chart + base

- [x] 4.1 `values.yaml`: `broker.poolRuntimeKey: ""`, `warmPool.templateName:
      code-standard-ephemeral-v1`, `warmPool.replicas: 1`
- [x] 4.2 `templates/pool-key-secret.yaml` + `templates/sandboxtemplate-pool.yaml`
      (v1 + pool-key volume/mount, pool template name)
- [x] 4.3 `warmpool.yaml` templateRef → pool template
- [x] 4.4 values.schema.json for new keys

## 5. Tests

- [x] 5.1 resolve: claim created + bound-sandbox labels/annotation; warm-pool
      disabled fallback; bind-timeout 503
- [x] 5.2 reaper: claimed ephemeral deletes claim; unclaimed deletes sandbox
- [x] 5.3 key fallback: pool key used only with label; fail-closed otherwise

## 6. Docs + spec + verification

- [x] 6.1 spec delta `specs/broker/warm-pool-claiming/spec.md`
- [x] 6.2 `docs/operations.md` (warm pool section rewrite, auth tradeoff),
      `docs/deploy.md` (new values), `docs/architecture.md` claim note fix
- [x] 6.3 `cargo fmt/clippy/test --workspace`, `helm lint` + template render
