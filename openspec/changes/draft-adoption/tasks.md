# Tasks

- [x] `BROKER_DRAFT_ADOPTION_WINDOW_SECONDS` config (default 21600, 0=off) + chart values/schema/env + docs
- [x] `SandboxStore::move_workspace_dir` + `WorkspaceMove` (batch/v1 Job, ttl, backoff 0, poll-to-completion; stub records)
- [x] `capture_draft_adoption` in resolve (gated: create-only, persistent, PVC tier, no S3, fresh draft; hash subpaths; runtime image) + post-readiness best-effort execution
- [x] Metric `owui_broker_draft_adoptions_total{result}` (adopted/skipped_no_draft/skipped_stale/failed)
- [x] Broker RBAC += `batch/jobs` create/get/delete (chart + deploy/base)
- [x] 4 unit tests (fresh/stale/window-0/no-draft) + e2e `test_draft_adoption.py` (follows chat, second chat empty, survives re-resolve)
- [x] Rate limits raised 20→30 rps / 40→60 burst (defaults + chart + schema) — FileNav polling saturates 20/s
- [x] #150 resume-race fix (digest-verified live): touch last-used at resume-patch time so the reaper can't re-park mid-boot
- [x] Gates: 357 cargo tests, fmt/clippy clean, helm lint + render valid, mkdocs strict, KIND per-user-pvc lane 6/6
