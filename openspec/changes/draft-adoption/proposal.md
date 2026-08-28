# Proposal: Draft adoption — files uploaded in an id-less new chat follow the chat

## Why

OWUI v0.11 assigns the chat id server-side on the first message; before that
the terminal sends no `X-Session-Id` and the broker keys the sandbox by user
alone (draft sandbox). When the id materializes, a different sandbox with a
fresh workspace subPath resolves and the user's uploads appear to vanish
(#157 — verified in production; data intact on the per-user PVC).

## What Changes

- `shared::BrokerConfig::draft_adoption_window_seconds`
  (`BROKER_DRAFT_ADOPTION_WINDOW_SECONDS`, default 21600, 0 disables).
- `resolve_sandbox`: after `wait_for_ready`, when this resolve is *creating* a
  new persistent/PVC chat sandbox, `s3_restore` is None, and the user's draft
  sandbox (`sandbox_name(user, user)`) has `broker-last-used` within the
  window — run a one-shot batch/v1 Job mounting the workspace claim root that
  moves the draft subPath contents (minus the reserved `.open-websandbox`
  dir) into the new chat subPath. Blocks readiness like S3 restore; a failed
  adoption logs + counts and never fails the resolve.
- `SandboxStore::run_job`: create + poll-to-completion + cleanup (kube impl;
  stub records invocations for unit tests).
- Broker RBAC += `batch/jobs` `create/get/delete` (chart + deploy/base).
- Metric `owui_broker_draft_adoptions_total{result}`.
- e2e: `tests/e2e/test_draft_adoption.py` on the per-user-pvc kind lane.
- Docs: deploy.md env table, operations.md flow, architecture.md, CHANGELOG.

## Impact

Additive; no API/CRD changes. Works for per-user-pvc and shared-subpath
modes on RWX (best-effort on RWO — Job may fail to attach; skipped, logged).
Draft sandbox keeps its reserved dir; adoption runs exactly once per chat
(creation-time only).
