# Proposal: cut terminal-open latency (pod-direct readiness + probe + park tuning)

## Why

Opening a terminal on a cold/parked chat takes 3–6s. Measured end-to-end on
a production deployment (2026-10-20, resume-from-parked path):

| Phase | Time | Cause |
|---|---|---|
| WS open → auth → `operatingMode` patch | ~0.3s | K8s API round-trips |
| Controller creates pod → container start | ~0.6s | scheduling |
| Runtime boot | 1ms | — |
| Readiness probe pass | 0.7–2s | `periodSeconds: 2` phase |
| Controller mirrors Ready → `Sandbox.status` | **~2.4s** | controller-runtime workqueue exponential backoff (reconciles 33.25s → 34.39 → 35.40 after pod Ready at 33.0) |
| Broker 250ms poll + key + `ensure_pty` + upstream WS | ~0.3s | — |

Two structural problems plus one policy problem:

1. The broker only needs `pod_ip`, but it waits for `Sandbox.status.Ready`,
   which the upstream v0.5.6 controller propagates 1–3s after the pod is
   actually Ready (workqueue rate-limit backoff). The Pod object itself has
   `Ready=True` + `podIP` ~2.4s earlier.
2. The runtime readiness probe polls every 2s, adding 0–2s (average ~1s).
3. `BROKER_PARK_IDLE_SECONDS=120` parks (deletes the pod of) any chat idle
   2 minutes, so nearly every terminal open is a cold resume rather than a
   warm resolve (<0.5s: one store GET + ~25ms relay).

## What Changes

1. **Broker resolves the Pod object directly** (`broker.pod-direct`):
   `SandboxStore::get_pod(name)` (pod name == sandbox name, same namespace,
   verified against live controller behavior). `wait_for_ready` now checks
   the pod first each tick — `Ready=True` + `podIP` short-circuits without
   waiting for `Sandbox.status`. The Sandbox poll remains as fallback (pod
   missing / future controller naming) and still supplies the
   `S3_LAST_KEY_ANNOTATION` read on success. Timeout diagnostics gain a
   pod-condition digest (e.g. `Pending; PodScheduled=False
   reason=Unschedulable: …`).
2. **Probe tuning**: SandboxTemplate readiness probe `periodSeconds: 2 → 1`
   (chart + `deploy/base`). Expected saving 0.5–1s on cold path.
3. **Park tuning**: `broker.parkIdleSeconds` default `120 → 900` (chart
   `values.yaml` + `deploy/base/broker.yaml` + schema description). Chats
   revisited within 15 min open warm. Cost: idle pods hold their requests
   (250m CPU / 512Mi) longer; single-user-scale clusters accept this, and
   `reapSeconds` (7d) is unchanged.

Expected result: warm open <0.5s (unchanged), cold resume ≈ 2s (was 4.5s).

Non-goals: wiring `SandboxWarmPool` claiming (separate change), gVisor vs.
runc deployment choices (deployment-specific, see docs/deploy.md), replacing
the 250ms poll with a watch (≤0.25s gain, not worth the complexity now).

MCP/frontend: no API surface changes — internal resolve path and Helm
defaults only; existing MCP tooling unaffected.

## Impact

- `rust/broker/src/store.rs` — trait + KubeStore + stub `get_pod`
- `rust/broker/src/resolve.rs` — pod-first `wait_for_ready`, pod digest
- `rust/broker/src/resolve.rs` tests — pod-ready-before-status case, fallback
- `open-websandbox-platform/chart/templates/sandboxtemplate.yaml`,
  `values.yaml`, `values.schema.json` — probe + park defaults
- `open-websandbox-platform/deploy/base/{broker,sandboxtemplate-code-standard}.yaml`
  — byte-parity with chart render
- `docs/operations.md` — latency budget + tuning notes
- specs: `broker/resolve.md` (new spec: pod-direct readiness contract)
