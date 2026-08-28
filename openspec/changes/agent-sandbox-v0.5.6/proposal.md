# Proposal: agent-sandbox v0.5.6

## Why

The vendored upstream `kubernetes-sigs/agent-sandbox` is pinned at v0.5.3; v0.5.6
is latest. Investigation (#153) showed the delta is low-risk for us: CRDs are
unchanged except the additive `SandboxWarmPool.status.observedGeneration`; the
v0.5.4 `Suspended`-condition semantics change does not touch our broker (it reads
only `type=Ready` and drives park/resume via `spec.operatingMode`); the self-built
router gains scoped-token authz and warm-pool routing fixes for free.

## What Changes

- `open-websandbox-platform/upstream/`: vendor `sandbox-with-extensions-v0.5.6.yaml`
  (now plain multi-doc, not `kind: List`), refresh `VERSION` + `SHA256SUMS`, drop v0.5.3.
- Regenerate `chart/crds/upstream-agent-sandbox-v0.5.6-crds.yaml` and
  `chart/files/upstream-agent-sandbox-v0.5.6.yaml` from it (recipes updated for
  multi-doc input); update the embedding template + values comment.
- Workflows: 7 upstream checkout pins + router build labels + `integration.yml` CRDs path.
- `rust/broker/tests/kube_live.rs` provenance header path.
- Docs: quickstart, deploy (incl. correcting the stale "upstream publishes only
  :latest" router note — no router image is published at all), operations upgrade
  section, index, NOTICE, AGENTS.md, CHANGELOG.
- No broker/runtime code changes.

## Impact

Controller image bump + one RBAC rule added; CRD apply is additive (safe
in-place). Rollback = re-apply the v0.5.3 manifest per operations.md. Verified by
the full e2e matrix plus a live KIND run (smoke + PVC tiering) on the v0.5.6
controller and a v0.5.6-built router.
