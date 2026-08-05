# Tasks — release v0.1.0

Make open-sandbox releasable. Behavior unchanged. Cite `proposal.md` decisions (D#).
Legend: `[ ]` todo · `[~]` in flight · `[x]` done.

## Tests (pytest + KIND)

- [ ] test infra: `pyproject.toml` (`asyncio_mode=auto`), `requirements-test.txt`, conftest
- [~] `tests/unit/runtime/` — `_safe_path` traversal, `/files/*` round-trip, `/execute`
      (exit/timeout/group-kill), WS terminal echo (real fs + PTY)
- [ ] `tests/unit/broker/` — name hashing, session resolution, staging migration, reaper
      (fake k8s client; monkeypatch `api`/`core` globals)
- [ ] gVisor/runc toggle via Helm value `sandboxTemplate.runtimeClassName` (default gvisor; `""` for KIND) — no broker code change needed
- [ ] `tests/e2e/values-runc.yaml` (sets runtimeClassName="" for KIND)
- [ ] `tests/e2e/` — KIND: controller + CRDs + platform; create sandbox; `/execute`; assert
- [ ] `@pytest.mark.gvisor` manual smoke (`scripts/smoke-gvisor-sandbox.yaml`)

## Packaging — Helm chart (D1, D2)

- [~] `agent-sandbox-platform/chart/` (Chart.yaml + values.yaml + templates/) reproducing
      the live manifests exactly, with knobs: imageRegistry/owner/tag, broker env,
      runtimeClassName (gVisor/runc), warm pool, PVC/storageClass, idle TTLs
- [ ] prod values: imageRegistry=ghcr.io, imageOwner=<owner>, imagePullPolicy=IfNotPresent
- [ ] `helm lint` + `helm template` green
- [ ] render `manifests-v0.1.0.yaml` from the chart at release

## Router self-build (D3)

- [ ] CI job: clone `agent-sandbox@v0.5.3`, build sandbox-router, push digest-pinned image

## CI — `.github/workflows/`

- [ ] `ci.yml` — ruff + pytest unit (push)
- [ ] `e2e.yml` — KIND runc e2e (PR)
- [ ] `release.yml` — build+publish 3 images + render manifests (on tag)

## Docs

- [~] README rewrite (open-sandbox, quickstart) + `docs/architecture.md`, `docs/deploy.md`,
      `docs/operations.md`, `docs/security.md`
- [ ] `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`

## Versioning

- [ ] tag `v0.1.0`; `CHANGELOG.md` entry
