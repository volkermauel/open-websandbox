# Tasks: open-terminal compat stage 3 (v0.17.x / OWUI 0.13.x)

## G1 — plumbing

- [ ] 1. `/api/config` features map: add `git`/`events`/`agents` (honest flags) to `runtime_config_json`
- [ ] 2. operationId alignment: `run_command`, `list_files`, `grep_search`, `get_process_status`, `list_processes`, `send_process_input`, `kill_process`, `read_user_terminal`-family gating names; update broker openapi snapshot test
- [ ] 3. `POST /execute` → upstream log shape (`id`, `command`, `cwd`, `status`, `exit_code`, `output[]`, `truncated`, `next_offset`) with `wait`/`tail` query
- [ ] 4. Background-process surface: `GET /execute` (list), `GET /execute/{id}/status` REST+WS, `POST /execute/{id}/input`, `DELETE /execute/{id}`, WS `/execute` (list stream)
- [ ] 5. `ExecuteRequest`: accept `cwd` + `env`; resolve `cwd` against session cwd like upstream
- [ ] 6. `/files/write` → `{path, size, created, patch?}`; `/files/replace` → +`patch?` (unified diff, size-capped like upstream `edit_patch`)
- [ ] 7. `/files/list` entries + `created` (birthtime via `stat -c %.9Y` batch fallback, else null)
- [ ] 8. `/system` prompt refresh to upstream-verbatim @ v0.17.x (display_file paragraph) + pinned test
- [ ] 9. Terminal WS protocol: auth `resume`/`cursor`/`flow_control`/`process_info`; frames `flow_control`, `output{start,end,truncated}`+binary, `process{pid,name}`, `ack`, `activity`, `chat` (bind/rebind rules), tolerate `ping`

## G2 — flag-gated panels

- [ ] 10. `/git/*`: status/diff/log/show/branches/worktrees + stage/unstage/commit/branch/checkout/uncommit/pull/fetch/push/worktree-create + `pr/capabilities`/`pr/*`/`issue/*` (gh); shapes ported from `utils/git.py`; flip `features.git`
- [ ] 11. WS `/events`: `fs:ready`/`fs:change` (workspace-root scoped) + `ports:change`; subscribe frames; broker WS relay; flip `features.events`
- [ ] 12. `GET /system/info`: cgroup-v2 + /proc snapshot in upstream shape (`setup` only when setup-script knob active)
- [ ] 13. `OPEN_TERMINAL_START_DIR` + `OPEN_TERMINAL_SETUP_SCRIPT` runtime knobs (setup status surfaced via `/system/info`)
- [ ] 14. Desktop sidecar image bump (chart default + docs; fork rebuild note)

## Docs / release

- [ ] 15. `docs/compatibility.md` matrix + divergences for the new surface
- [ ] 16. release.yml compat tags (`open-terminal-v0.17.21`, `owui-v0.13.4`) + CHANGELOG
- [ ] 17. e2e: pin OWUI v0.13.4; add per-flag probes where feasible without a cluster (collect-only must stay green)

## Deferred (G3, per proposal)

- `/agents` surface, `/notebooks`, Terminals-orchestrator protocol
