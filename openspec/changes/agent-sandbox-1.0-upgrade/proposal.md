# Proposal: upgrade vendored agent-sandbox v0.5.6 → v1.0.7

## Why

We pin agent-sandbox v0.5.6 (vendored, SHA-256-recorded). Upstream released
v1.0.0 … v1.0.7 (2026-08-28 → 2026-10-15). Research
(`research.md` in this directory, primary-source cited) shows a **low-risk,
additive** delta with real gains for us:

- **v1alpha1 + conversion webhook removed** (v1.0.0) — we only ever speak
  `v1beta1`; the webhook Service/certs/Role disappear from the vendored slice
  (one less failure mode in restricted networks).
- **Warm-pool scheduling-gate hold** (v1.0.7): members waiting on scheduling
  gates are held instead of false-"stuck" deleted — protects pool members
  during slow image pulls.
- **Warm-candidate grace period** (v1.0.6, `--sandbox-claim-warm-candidate-grace-period`,
  default 2s): tunable fallback window before a claim cold-starts.
- **Terminal `InvalidConfiguration`** classification (v1.0.3): permanent
  errors stop hot-looping and surface as a condition — enables broker fast-fail.
- kube Events from the Sandbox reconciler (v1.0.4), pool size metric,
  status-mirroring reliability fixes (v1.0.5–v1.0.7).

No group/version changes; claim and warm-pool specs unchanged;
`SandboxClaim.status.sandbox.serviceFQDN` is the only new field (serde-ignored
by kube-rs). `Suspended` condition semantics improved (never removed, reports
`NotSuspended`) — our broker reads conditions for error text only.

## What Changes

- Vendored CRDs: `chart/crds/upstream-agent-sandbox-v0.5.6-crds.yaml` →
  `…-v1.0.7-crds.yaml` (verbatim from the SHA-256-recorded release asset).
- Vendored controller slice: `chart/files/…-v0.5.6.yaml` → `…-v1.0.7.yaml`
  (webhook Service/namespaced Role/RoleBinding gone upstream; controller
  ClusterRole loses `customresourcedefinitions` get/patch/update — that was
  the v0.5.x storage migration).
- `upstream/sandbox-with-extensions-v1.0.7.yaml` + `VERSION` + `SHA256SUMS`
  refresh (asset sha256 `136c10c9…`).
- Docs: pin references v0.5.6 → v1.0.7 (README diagram, deploy/quickstart/
  operations/index/architecture). Router self-build stays on upstream
  `v0.5.6` source (compatible; re-pin tracked as follow-up).
- Broker code: **none required** (group/version unchanged, additive schema).
- Cluster upgrade procedure (ops runbook): CRDs first → verify all four
  `status.storedVersions == [v1beta1]` → roll controller Deployment → delete
  orphaned webhook objects (v1.0.0 upgrade step 4).

Non-goals: router re-pin to v1.0.x (authz/namespace changes upstream;
follow-up), enabling new controller flags (per-flag, later), broker
`InvalidConfiguration` fast-fail (small follow-up).

MCP/frontend: no API surface change (controller-internal upgrade).

## Impact

- `open-websandbox-platform/chart/crds/`, `chart/files/`, `chart/templates/upstream/`
- `open-websandbox-platform/upstream/{VERSION,SHA256SUMS,*.yaml}`
- `README.md`, `docs/*` pin references
- KIND e2e: controller image flows from the chart — no test changes expected
- Ops cluster runbook (private ops repo): CRD + controller apply order
