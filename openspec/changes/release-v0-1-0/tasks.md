# Tasks — release v0.1.0

Make open-sandbox releasable. Behavior unchanged. Cite `proposal.md` decisions (D#).
Legend: `[ ]` todo · `[~]` in flight · `[x]` done.

## Tests (pytest + KIND)

- [x] test infra: `pyproject.toml` (`asyncio_mode=auto`, coverage `fail_under=95`), `requirements-test.txt`, conftest (broker + runtime)
- [x] `tests/unit/runtime/` (122 tests, **100% branch**) — `_safe_path` traversal, `/files/*` round-trip + move/replace/archive/upload + error paths, `/execute` (exit/timeout=124/oversize-truncate), WS terminal write/disconnect/resize, create_terminal 503 (openpty/Popen fail), receiver message-type branches
- [x] `tests/unit/broker/` (104 tests, **100% branch**) — name hashing, auth, session resolution, ephemeral/persistent sandbox get-or-create, parked-sandbox resume, staging migration (all branches), proxy + redirect rewrite, terminal WS proxy (+ relay send-fail/ends-cleanly), park/reap loop, resolve/ensure retry loops, fresh-claim skip
- [x] **Combined: 100% branch** (997 stmts, 274 branches) across broker + runtime; `# pragma: no cover`/`no branch` only on async PTY/WS paths proven unreachable (Linux PTY EIO, Starlette disconnect-as-message, cancel-before-loop-exit) + defence-in-depth guards
- [x] gVisor/runc toggle via Helm value `sandboxTemplate.runtimeClassName` (default gvisor; `""` for KIND) — no broker code change needed
- [x] KIND profiles: `chart/values-kind.yaml` (runc) + `chart/values-kind-gvisor.yaml` (gVisor node)
- [x] `tests/e2e/` (5 tests, green under gVisor KIND) — controller + CRDs + Helm install; `/healthz`, `/execute`, `/files` write/read, workspace persistence
- [x] `@pytest.mark.gvisor` manual smoke (`scripts/smoke-gvisor-sandbox.yaml`) + `scripts/setup-kind-gvisor.sh`

## Packaging — Helm chart (D1, D2)

- [~] `agent-sandbox-platform/chart/` (Chart.yaml + values.yaml + templates/) reproducing
      the live manifests exactly, with knobs: imageRegistry/owner/tag, broker env,
      runtimeClassName (gVisor/runc), warm pool, PVC/storageClass, idle TTLs
- [ ] prod values: imageRegistry=ghcr.io, imageOwner=<owner>, imagePullPolicy=IfNotPresent
- [ ] `helm lint` + `helm template` green
- [ ] render `manifests-v0.1.0.yaml` from the chart at release

## Router self-build (D3)

- [x] CI job: clone `agent-sandbox@v0.5.3`, build sandbox-router, push image  *(release.yml build-router)*

## CI — `.github/workflows/`

- [x] `ci.yml` — ruff + pytest unit (push)  *(code is ruff-clean)*
- [ ] `e2e.yml` — KIND runc e2e (PR)
- [x] `release.yml` — build+publish 3 images (on tag)  *(router self-built from upstream@v0.5.3)*

## Docs

- [~] README rewrite (open-sandbox, quickstart) + `docs/architecture.md`, `docs/deploy.md`,
      `docs/operations.md`, `docs/security.md`
- [ ] `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`

## Versioning

- [ ] tag `v0.1.0`; `CHANGELOG.md` entry
