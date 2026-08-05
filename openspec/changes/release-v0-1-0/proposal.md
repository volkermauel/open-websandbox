## Why

open-sandbox is built + proven on prod but not **releasable**: no tests, no CI, no real
packaging (`deploy/base/` is loose YAML, not Kustomize), images hand-loaded
(`imagePullPolicy: Never`), the Go router built ad-hoc from upstream `:latest`, no
versioning, sparse docs. This change makes it release-ready at **v0.1.0** without changing
runtime behavior.

## Proposal

Deliver a releasable v0.1.0:

- **Tests** — pytest unit tests for the runtime + broker (no cluster needed; real fs + PTY)
  and a **KIND** e2e suite on **runc** (gVisor/runsc cannot nest in KIND — upstream's own
  e2e uses runc). gVisor-specific checks stay manual (`@pytest.mark.gvisor`).
- **Packaging** — a real Kustomize root (`deploy/base/kustomization.yaml` + `overlays/prod`);
  images published to a registry with `imagePullPolicy: IfNotPresent`, digest-pinned.
- **Router self-build** — CI builds the sandbox-router from `kubernetes-sigs/agent-sandbox`
  at a pinned tag (v0.5.3) and publishes a digest-pinned image (replaces ad-hoc `:latest`).
- **CI** (GitHub Actions) — `ci.yml` (ruff + pytest unit, every push), `e2e.yml` (KIND, PR),
  `release.yml` (build+publish + render manifests, on tag).
- **Docs** — README, architecture, deploy guide, operations/runbook, security model.
- **Versioning** — semver tag `v0.1.0`, `CHANGELOG.md`.

## Decisions

- **D1** Packaging: Kustomize base + overlays; **Helm deferred** (no divergent deployments yet).
- **D2** Registry: `ghcr.io/<owner>/open-sandbox-{broker,runtime,router}`; owner resolves via
  `github.repository_owner` in CI, configurable in the prod overlay.
- **D3** Router: **self-build from upstream `agent-sandbox@v0.5.3`** in CI (pinned), not vendored.
- **D4** e2e: **KIND + runc**; gVisor tests manual — `runsc` cannot nest in KIND.
- **D5** v0.1.0 scope = tests + packaging + CI + docs + router self-build. **Deferred**: Helm,
  cosign/SBOM, GitOps, dedicated/tainted nodes, full gVisor e2e, Prometheus.

## Impact

New: `tests/`, `docs/`, `.github/workflows/`, `deploy/base/kustomization.yaml` + overlays,
`CHANGELOG.md`, a `BROKER_RUNTIME_CLASS` env knob (lets e2e drop gVisor). No change to
running behavior. Project identity renamed to **open-sandbox**.
