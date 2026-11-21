# Tasks: open-terminal compat stage 3 (v0.17.x / OWUI 0.13.x)

## G1 — plumbing

- [x] 1. `/api/config` features map: `git` (binary probe) / `events` / `agents` (false) — keys SHADOW the sidecar's claims
- [x] 2. operationId alignment: `run_command`, `list_files`, `grep_search`, `get_process_status`, `list_processes`, `send_process_input`, `kill_process`, `upload_file`, `replace_file_content`, `read_user_terminal`, `send_user_terminal_input`; snapshot regenerated
- [x] 3. `POST /execute` → upstream log shape (`id`, `command`, `cwd`, `status`, `exit_code`, `output[]`, `truncated`, `next_offset`) with `wait`/`tail` query
- [x] 4. Background-process surface: `GET /execute` (list), `GET /execute/{id}/status` REST+WS, `POST /execute/{id}/input`, `DELETE /execute/{id}`, WS `/execute` (list stream); broker WS relays
- [x] 5. `ExecuteRequest`: accepts `cwd` + `env`; `cwd` resolves against the workspace base
- [x] 6. `/files/write` → `{path, size, created, patch?}`; `/files/replace` → +`patch?` (upstream `edit_patch` port with the size cap)
- [x] 7. `/files/list` entries + `created` (statx via std, GNU-stat batch fallback, null)
- [x] 8. `/system` prompt refresh to upstream-verbatim @ v0.17.21 (display_file paragraph) + pinned test
- [x] 9. Terminal WS protocol: auth `resume`/`cursor`/`flow_control`/`process_info`; frames `flow_control`, `output{start,end,truncated}`+binary, `process{pid,name}`, `ack`, `activity`, `chat` (bind/refuse), tolerate `ping`; broker forwards the sanitized hello

## G2 — flag-gated panels

- [x] 10. `/git/*`: full surface ported from `utils/git.py` (status/diff/log/show/branches/worktrees + all actions + pr/issue via gh); `features.git` = binary probe
- [x] 11. WS `/events`: `fs:ready`/`fs:change` (fingerprint poll + git markers) + `ports:change`; subscribe validation 4000/4003; `features.events` = true; broker relay already targeted the runtime
- [x] 12. `GET /system/info`: cgroup-v2 + /proc snapshot in the upstream shape (2 s cache; delta cpu percent)
- [x] 13. `OPEN_TERMINAL_START_DIR` + `OPEN_TERMINAL_SETUP_SCRIPT` runtime knobs (setup status surfaced via `/system/info`)
- [x] 14. Desktop sidecar: pin stays 0.16.1 (works; rebuild-on-0.17.x documented as a follow-up in `docs/compatibility.md`)

## Docs / release

- [x] 15. `docs/compatibility.md` — rewritten matrix for v0.17.21/OWUI 0.13.4, WS protocol section, divergences, orchestrator note
- [x] 16. release.yml compat tags (`open-terminal-v0.17.21`, `owui-v0.13.4`) + CHANGELOG
- [x] 17. e2e: stage-3 `exec_out()` helper + `?wait=` on every exec POST (collect-only green; full run needs a KIND cluster — R1)

## Deferred (G3, per proposal)

- `/agents` surface, `/notebooks`, Terminals-orchestrator protocol
