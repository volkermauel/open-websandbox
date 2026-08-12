# Tasks — per-session runtime key

## Phase 1 — investigation notes (this change)
- [x] Read CRDs (Sandbox / SandboxClaim / SandboxTemplate / SandboxWarmPool), broker, runtime,
      chart, NetworkPolicy. Document the delivery decision + constraints in `proposal.md`.

## Phase 2 — runtime per-session key (file-backed)
- [ ] `runtime/server.py`: read key from `/etc/runtime-key/api-key`; boot guard + request guard;
      reload-on-mismatch; remove shared `RUNTIME_API_KEY`.
- [ ] `tests/unit/runtime/test_runtime_auth.py`: file-backed key; keep route-table invariant.

## Phase 3 — chart (Secret lifecycle + RBAC + template)
- [ ] `chart/templates/sandboxtemplate.yaml`: drop `RUNTIME_API_KEY` env.
- [ ] `chart/templates/broker.yaml`: drop shared `runtime-api-key` Secret + broker env; add
      `secrets` to broker Role; drop `$runtimeKey` resolution.
- [ ] `chart/values.yaml` (+ schema): drop `sandboxTemplate.runtimeApiKey`; `warmPool.replicas: 0`.

## Phase 4 — broker per-session key (stateless) + direct-Sandbox unification
- [ ] `broker/main.py`: per-session Secret helpers (mint/ensure/rotate/read/delete);
      `_runtime_auth_headers(sandbox_name)`; inject `runtime-key` volume; rotate-on-resume;
      ephemeral → direct `Sandbox`; remove `SandboxClaim` path + shared `RUNTIME_API_KEY`;
      reap key Secret; unify reaper on `sandboxes`.
- [ ] broker tests: per-session key + direct-Sandbox model + rotate-on-resume.

## Phase 5 — verify + ship
- [ ] `ruff check` + `python3 -m pytest tests/unit -q` green.
- [ ] `helm lint` + `helm template` (broker + runtime + per-session Secret/RBAC; vendored
      controller byte-identical).
- [ ] commit per phase, push branch, open PR; CI e2e (gvisor+runc) green.
