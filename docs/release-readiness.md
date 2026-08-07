# open-sandbox — Release, Production & Battle-Readiness Gaps

Synthesized from a 6-way parallel review (5 codebase lenses + an external benchmark
vs Coder / Gitpod / Eclipse-Che / agent-sandbox norms — see
[`production-readiness-checklist.md`](./production-readiness-checklist.md)).

**Tiers:** 🔴 P0 = blocks cutting v0.1.0 · 🟠 P1 = production-readiness (before real
tenants) · 🔵 P2 = battle-test / advanced. Effort: **S** ≤½ day · **M** ½–2 days · **L** 3+ days.

## Verdict

- **Releasable as v0.1.0** after the P0 list (~2–3 days). The code is functionally
  complete and unit-tested (100% branch); the gaps are release mechanics + one
  fail-open auth bug + adoption/onboarding.
- **Not yet production-grade.** P1 must land before real tenants: observability is a
  black box, `/healthz` causes silent outages, the broker is pinned to one replica,
  multi-tenant isolation has **no negative test**, and per-user PVCs have no backup story.
- **Not battle-tested.** No load/soak/chaos suite, no stateful upgrade/rollback e2e,
  no long-session/120s-suspend coverage.
- **Scope tension to decide:** the v0.1.0 plan (release OpenSpec D5) *defers*
  cosign/SBOM, per-tenant OIDC, and HA broker — but the external benchmark lists all
  three as "credible-v1.0 table-stakes." Reconcile the intended meaning of v0.1.0
  ("functional first cut" vs "production v1.0") before tagging.

## P0 — 🔴 v0.1.0 release blockers

| # | Gap | Area / file | Fix | Effort |
|---|-----|-------------|-----|--------|
| 1 | **No GitHub Release / Helm chart published** | `.github/workflows/release.yml` | add `helm package` + `helm push oci://ghcr.io/<o>/charts`, `softprops/action-gh-release` attaching the `.tgz`; bump `permissions: contents: write` | **M** |
| 2 | **CHANGELOG.md absent** | repo root | seed Keep-a-Changelog with v0.1.0 | **S** |
| 3 | **Python deps unpinned** | `broker/requirements.txt`, `runtime/requirements-{app,common}.txt` | `uv pip compile` → exact versions (digests optional) | **S** |
| 4 | **No git remote → whole release path unverified** | repo | create repo, push, tag `v0.1.0-rc1`, dry-run tag→3 images→chart OCI→GitHub Release end-to-end | **S** (ops) |
| 5 | **`BROKER_SHARED_SECRET` fail-open + literal default** | `broker/main.py` (`_auth`), `chart/values.yaml` | **fail-closed**: refuse start if unset/placeholder; drop `dev-shared-secret-change-me`; generate 32-byte `randAlphaNum` on install | **S** |
| 6 | **No `imagePullSecret` path** | `chart/values.yaml` + `templates/broker.yaml` + `deploy.md` | add `imagePullSecrets: []` value + conditional block on all 3 deployments + a deploy.md `regcred` section | **M** |
| 7 | **README quickstart not copy-pasteable** | `README.md` | finalize registry owner (or a public sample image); numbered quickstart ending in a verified `GET /api/config` | **M** |

## P1 — 🟠 production-readiness (before real tenants)

| # | Gap | Area / file | Fix | Effort |
|---|-----|-------------|-----|--------|
| 8 | **Multi-tenant isolation unverified** (no negative tests) | `tests/e2e`, `conftest.py` (1 user/1 session) | add: user-B ✗ read user-A file (per-user-pvc + shared-subpath); peer-pod `:8888` denied; absolute-path traversal in `/files/read` | **S–M** |
| 9 | **Zero `/metrics`** | `broker/main.py`, `runtime/server.py`, `broker.yaml` | Prometheus `/metrics`; instrument reaper (last-run/parks/reaps/errors), resolve latency, proxy/WS status; ServiceMonitor + scrape annotations (match the router's `:9090`) | **M** |
| 10 | **`/healthz` process-up only → silent outage** | `broker.yaml`, `main.py` | split liveness (process+config) vs readiness (list sandboxclaims / reach `ROUTER_URL`); add `startupProbe`. Today an apiserver outage → 500s while `/healthz` stays green → pods never restart | **M** |
| 11 | **No graceful shutdown** | `broker/main.py` | `@app.on_event("shutdown")` cancels reaper, closes `httpx.AsyncClient`, drains in-flight WS; `terminationGracePeriodSeconds ≥ 45` | **M** |
| 12 | **Broker pinned to `replicas:1` (SPOF)** | `broker/main.py` (`_migrate_locks` per-process, `_reaper_loop` on every replica), `chart/templates/broker.yaml` | leader-election (CR-owner lease) before >1 replica; add PDB now; HPA later. >1 replica today → migrate races + reaper thundering-herd | **L** |
| 13 | **Migrate-leak window** | `broker/main.py` `_migrate_staging_to_chat` | staging-unreachable returns early **without clearing** → pre-chat uploads linger on the user PVC and leak into the next chat. Reaper sweep clears stale staging regardless of reachability | **S** |
| 14 | **Resolve poll-storm** | `broker/main.py` `resolve_sandbox` | 1s × 60s GET loop hammers apiserver under bursty arrivals. Switch to event-driven readiness (watch) + explicit `httpx.Limits` + container `ulimit -n` | **M** |
| 15 | **Per-user PVC backup/restore undocumented** | `docs/operations.md` | `profile.default=persistent` → irreplaceable RWX PVCs, no recovery path. Add backup/restore (snapshot schedule / namespace-scoped Velero); note PVCs unencrypted | **M** |
| 16 | **OpenAPI spec diverges from runtime** | `broker/openapi_spec.py` vs `runtime/server.py` | curated spec lists 5 routes; runtime exposes ~20 (`/files/*`, `/api/terminals`); the broker's own migrate calls paths **absent from the spec**. Reconcile, or scope it "curated LLM subset"; tie `info.version` to `appVersion` | **M** |
| 17 | **Broker PodSecurity not hardened** | `deploy/base/broker.yaml` | router/runtime are locked down (non-root, readOnlyRootFilesystem, drop ALL, seccomp); **broker is not**. Add `pod-security.kubernetes.io/enforce=restricted` + `securityContext` | **S** |
| 18 | **Single shared secret = cross-tenant impersonation** | `broker/main.py` (trusts `X-User-Id`) | any secret holder is any user. Per-tenant / short-lived OIDC-bound tokens + documented rotation/distribution (deferred in D5 — flag as known limitation if v0.1.0 ships without it) | **M** |
| 19 | **`design.md` has 4 open questions incl. D1** | `openspec/changes/adopt-agent-sandbox/design.md` | resolve D1 (dedicate+taint sandbox nodes vs co-tenancy with CNPG/argocd — security-relevant); convert to ADR (context/decision/consequences) | **S–M** |

Minor P1: `RUNTIME_API_KEY` inter-component auth hardening; IPv6 DNS egress rule missing
(`networkpolicy-runtime.yaml` IPv4-only); dev-scale quotas (50 PVC / 7-day reap ≈ ~20
MAU ceiling — raise for prod); `values.schema.json` so typos fail at install not runtime.

## P2 — 🔵 battle-test / advanced

- **No chaos / soak / load suite** — k6/locust WS+HTTP soak (1k sessions), kill
  broker/router/controller/etcd, drain-under-load, PVC detach/reattach, apiserver
  throttle injection. **L**
- **Stateful upgrade/rollback untested** — Helm-upgrade-with-retained-PVC + rollback e2e,
  version-skew doc. The controller/CRD is vendored upstream v0.5.3 (not team-owned) so
  conversion-webhook needs are upstream-driven. **M**
- **Node drain with live sandboxes unhandled** — eviction kills terminals; RWX reattach
  works cross-node but in-pod state + WS die with no resume. Test eviction under v0.5.3,
  add client WS reconnect/resume. **M–L**
- **120s-suspend & long sessions unexercised** — 1h-idle double-park-under-clock-skew. **M**
- **Benchmark "table-stakes" still deferred (D5)** — cosign-signed images + SBOM; per-tenant
  OIDC/RBAC; full gVisor e2e in CI. Re-evaluate vs your threat model. **M each**
- **Per-tenant egress filtering** (benchmark judgment call) — today egress is a single
  global allowlist; for adversarial-code sandboxes, per-tenant egress may be table-stakes.
  **M**
- **Advanced (benchmark `[A]`)** — multi-arch images, per-tenant node pools, 10k+ WS load
  test, community/process docs.

## Converged strengths (verified, keep)

`_safe_path` sound (realpath + startswith); `hmac.compare_digest` for HTTP + WS auth;
egress blocks RFC1918/link-local **incl. IMDS `169.254.169.254`** (anti-lateral-movement);
`automountServiceAccountToken: false`, `KUBERNETES_*` env blanked; gVisor `runtimeClassName`
enforced; runtime locked-down (uid 1000 / readOnlyRootFilesystem / cap-drop / seccomp);
router is the ops "gold standard" (separate healthz/readyz, `:9090/metrics`, PDB,
topologySpread, 45s grace); CI runs a real runsc KIND cluster; 100% branch unit coverage.

## Recommended critical path

1. **P0 #5 first** (fail-open auth — a ½-day security fix, don't tag without it).
2. **P0 #1–#4 + #6** in one release-prep PR (release publish + CHANGELOG + dep pins +
   git remote dry-run + imagePullSecret) → unblocks an actual `v0.1.0-rc1` cut.
3. **P0 #7 + P1 #8** (executable quickstart + the multi-tenant negative tests) → makes the
   release credible.
4. Then the **observability trio** (#9 metrics, #10 real probes, #11 graceful shutdown) +
   **#13 migrate-leak fix** → converts the broker from a black box into an alertable,
   self-healing component. This is the line between "v0.1.0 shipped" and "production-ready."
5. **P1 #12 (leader election)** unblocks HA before scaling beyond one broker replica.
