# Tasks — release v0.1.0

Make open-sandbox releasable. Behavior unchanged. Cite `proposal.md` decisions (D#).
Legend: `[ ]` todo · `[~]` in flight · `[x]` done.

## Tests (pytest + KIND)

- [ ] test infra: `pyproject.toml` (`asyncio_mode=auto`), `requirements-test.txt`, conftest
- [~] `tests/unit/runtime/` — `_safe_path` traversal, `/files/*` round-trip, `/execute`
      (exit/timeout/group-kill), WS terminal echo (real fs + PTY)
- [ ] `tests/unit/broker/` — name hashing, session resolution, staging migration, reaper
      (fake k8s client; monkeypatch `api`/`core` globals)
- [ ] `BROKER_RUNTIME_CLASS` env knob in broker so e2e can drop gVisor (D4)
- [ ] `deploy/test/` runc overlay (strip `runtimeClassName: gvisor`)
- [ ] `tests/e2e/` — KIND: controller + CRDs + platform; create sandbox; `/execute`; assert
- [ ] `@pytest.mark.gvisor` manual smoke (`scripts/smoke-gvisor-sandbox.yaml`)

## Packaging (D1, D2)

- [ ] `deploy/base/kustomization.yaml` (resources + `images:` transformer + namespace + labels)
- [ ] `deploy/overlays/prod/` (registry/owner, replicas, PVC size)
- [ ] images -> `ghcr.io/<owner>/open-sandbox-*`; `imagePullPolicy: Never` -> `IfNotPresent`
- [ ] render `manifests-v0.1.0.yaml` at release

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
