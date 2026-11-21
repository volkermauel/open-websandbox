# Proposal: open-terminal compatibility stage 3 — the v0.17.x / OWUI 0.13.x era

## Why

Both ends of the contract moved since stages 1–2 (#164/#169, pinned at
open-terminal **v0.12.3** @ `542094a`, verified against Open Web UI **v0.11.4**):

1. **The reference server moved**: `open-webui/open-terminal` is now at
   **v0.17.21** (2026-11-21). Route table, response shapes, the `/system`
   default prompt, and the terminal WebSocket protocol all grew.
2. **The caller moved**: Open Web UI is at **v0.13.4** (2026-11-17). Its
   terminal client grew feature-flag discovery, a Git panel, live file
   events, coding-agent-as-model support, and a processes dock that calls
   `/execute` REST/WS shapes directly.
3. **A new official orchestrator exists**: `open-webui/terminals` (v0.4.4)
   — template-scoped container orchestration (user/chat/model scopes) on
   Docker/Kubernetes, occupying the architectural slot our broker fills.
   OWUI 0.13.x changelog: *"Terminals 0.4.0 required … **Open Terminal and
   Tether terminal connections are not affected**"* — plain open-terminal
   connections (our lane) remain a supported first-class connection type.
   Becoming a Terminals-protocol orchestrator is a separate strategic
   decision (see "Orchestrator lane" below), not a requirement for
   compatibility.

**Status today** (OWUI 0.13.4 against us): Files tab + interactive terminal
work; `/openapi.json` tool dispatch still functions because OWUI dispatches
LLM tools generically from *our* spec. Everything else degrades silently
(hidden panels) or breaks visibly (processes dock, rich chat step cards,
edit-diff cards, user-terminal tools, Compare view).

## Evidence

- Upstream diff: `542094a..origin/main` in `~/git/open-terminal`
  (`open_terminal/main.py`, `utils/git.py`, `utils/events.py`, `utils/system.py`,
  `agents/` package; local clones `/tmp/main_base.py`, `/tmp/main_new.py`).
- Caller side: open-webui @ v0.13.4 (`/tmp/opencode/open-webui`),
  chiefly `backend/open_webui/utils/tools.py` (OpenAPI-driven tool dispatch),
  `utils/terminals.py`, `utils/skills.py`, `utils/agents/{discovery,runner,transport}.py`,
  `routers/tether.py`, `src/lib/apis/terminal/index.ts` (feature flags).
- OWUI/terminals changelogs: v0.13.0–v0.13.4 and terminals 0.4.0–0.4.4.

## Caller contract essentials (OWUI 0.13.4)

- `GET /api/config` → `features` map is the master gate:
  `terminal` (defaults on when absent), `system`, `git`, `events`, `agents`,
  `browser`, `desktop`. Missing flag = panel hidden. *"Missing flags mean
  unsupported; never probe a new endpoint on an older server."*
- `GET /openapi.json` → every LLM tool; dispatched by **operationId**.
  The chat renderer maps upstream operationIds (`run_command`,
  `get_process_status`, `read_file`, `write_file`, `replace_file_content`,
  `grep_search`, `list_files`, …) to rich step cards; mismatches render as
  generic blobs. `filter_user_terminal_tools` looks up
  `read_user_terminal` / `send_user_terminal_input` **by name**.
- Frontend calls some REST shapes directly (not via spec): processes dock
  (`GET /execute`, `GET /execute/{id}/status`, both also as WS with
  `next_offset`), `POST /execute?wait=1&tail=20` for the Files-tab git
  clone, `GET /api/terminals/user/output` probe, `/files/compare` Compare
  view, `/system/info` modal, `/git/*` panel, WS `/events`
  (`fs:ready`/`fs:change`/`ports:change`).
- Terminal WS first message now sends
  `{"type":"auth","token","chat_id","resume":true,"cursor":N,"flow_control":true,"process_info":true}`
  and expects: `flow_control` confirm frame, output frames
  `{"type":"output","start","end","truncated"}` followed by a binary chunk,
  `{"type":"process","pid","name"}` on foreground-process change, plus
  client frames `ack{cursor}` / `activity{active}` / `chat{chat_id}` /
  `ping` (25 s keepalive) / `resize`. Close codes 4001/4003/4004 have
  specific meanings (no-reconnect vs reconnect-with-backoff).
- Graceful degradation is broad (all flag-gated panels, Compare 405/404,
  git 404, events→polling fallback, skills→[], user-terminal probe failure
  → tools removed) but NOT universal: the processes dock and git-clone
  parse upstream `/execute` shapes directly; agent carrier WS is strict
  (sequence gaps are fatal per turn).

## Gap analysis

### Already compatible (no work)

`/files/list|read|write|mkdir|move|delete|view|serve|grep|glob|search|matches|
archive|upload|display|cwd`, `/ports`, `/proxy/{port}`, `/health`,
`/info`, `/skills*`, `/files/compare` (shape-compatible; no document
extraction — documented divergence), desktop/browser lane via the sidecar
(transport `frames`/`vnc`, `desktop/audio` — sidecar ≥ 0.16.0 features).

### G1 — plumbing (required for correct OWUI 0.13 operation)

| # | Change | Upstream since | Notes |
|---|--------|----------------|-------|
| 1 | `/api/config` features map: add `git`, `events`, `agents` (+ verify sidecar merge covers `browser`/`desktop`) | 0.17-era | Keys must be **honest** (`false` until the surface ships) — OWUI treats truthiness strictly |
| 2 | operationId alignment in the utoipa spec: `execute`→`run_command`, `list_dir`→`list_files`, `grep`→`grep_search`, plus `get_process_status`/`list_processes`/`send_process_input`/`kill_process` once G2 lands | — | Update `rust/broker/tests/openapi.snapshot.json`; renderer + user-terminal-tool gating key off exact names |
| 3 | `POST /execute` response: upstream log shape `{id, command, cwd, status, exit_code, output:[{type,data}], truncated, next_offset}` (+ `log_path` optional) — ours returns `{stdout, stderr, exit_code, timed_out}` | v0.12.3 already | Frontend git-clone + processes dock parse this directly; LLM tool results must be `{status, output, exit_code}` for the step card |
| 4 | Background-process surface: `GET /execute` (list), `GET /execute/{id}/status` REST+WS (`offset`/`tail`/`wait`), `POST /execute/{id}/input`, `DELETE /execute/{id}`, WS `/execute` | v0.12.3 | Biggest G1 item: jsonl process logs, per-reader offsets, expiry (upstream: session-scoped, TTL cleanup) |
| 5 | `/files/write` response `{path, size, created:bool, patch?}`; `/files/replace` + `patch?` (unified diff, capped) | 0.17.20 | Edit-diff cards in chat read `patch`; fallback path uses `created` |
| 6 | `/files/list` entries + `created` (birthtime; GNU `stat` batch fallback; `null` ok) | 0.17-era | |
| 7 | `/execute` request body: accept `{command, cwd, env}` (+ `wait`/`tail` query) — ours only reads `{command, timeout}` | 0.17.7-era `env`, older `cwd` | OWUI git-clone sends `env` (`GIT_TERMINAL_PROMPT=0`); `cwd` resolution via session cwd already exists |
| 8 | `/system` default prompt: refresh pinned copy to current upstream (adds the `display_file` delivery paragraph) | 0.17-era | Update `rust/runtime/src/system.rs` literal + provenance comment together (pinned test forces the decision) |
| 9 | Terminal WS protocol: `resume`+`cursor`+`flow_control` auth (256 KiB window, bounded history), `output{start,end,truncated}` framing, `process{pid,name}` frames (`process_info`), tolerate `ping`, `ack`/`activity`/`chat` frames | 0.16–0.17.x | `chat` rebind interacts with our sandbox-per-chat model: accept only same-chat/unbind (else upstream's `{"type":"chat","error":"Terminal already belongs to another chat"}`); `resume` works naturally (PTY buffer already in runtime) |

### G2 — flag-gated panels (soft-degrade today, expected by 0.13 UI)

| # | Change | Notes |
|---|--------|-------|
| 10 | `/git/*` surface (status, diff, log, show, branches, worktrees, stage/unstage/commit/branch/checkout/uncommit/pull/fetch/push/worktree-create, `pr/capabilities`, `pr/*`, `issue/*`) | Port shapes from `utils/git.py`; shell out to `git`/`gh` inside the sandbox (workbench image already has git; add `gh`); `features.git:true` only after; `pr/*`+`issue/*` require `gh` auth (report `available:false` otherwise) |
| 11 | WS `/events` — auth frame + `{"type":"subscribe","path"|"ports"}` → `fs:ready`, `fs:change` (no filenames), `ports:change` | Notify/inotify in runtime; workspace-root-scoped; broker needs a WS relay like terminals'; `features.events:true` after |
| 12 | `GET /system/info` — machine snapshot (`hostname, os, arch, environment, uptime_*, cpu{percent,cores}, memory{total,used}, disk, processes[top], process_scope, setup?`) | cgroup v2 + /proc parsing in Rust (gVisor exposes cgroups); no `setup` unless we implement `OPEN_TERMINAL_SETUP_SCRIPT` equivalent (0.17.7 — worth doing for template parity) |
| 13 | `OPEN_TERMINAL_START_DIR` + `OPEN_TERMINAL_SETUP_SCRIPT` runtime knobs | 0.17.7; used by Terminals templates ("start directory and setup script need Open Terminal 0.17.7+"); cheap and improves parity |
| 14 | Desktop sidecar image bump (fork rebuild on 0.17.x: audio hardening, window-drag/input-order fixes 0.17.18, XFCE-default 0.17.15) | Optional but cheap follow-up; `desktop`/`browser` flags already flow from the sidecar config merge |

### G3 — deliberately deferred (document, don't build)

| # | Surface | Reason |
|---|--------|--------|
| 15 | `/agents` catalog + `/agents/{provider}/sessions` + WS carrier (`protocol_version:1`, strict sequence, resume/fork echo, 14 provider integrations + `OPEN_TERMINAL_ACP_AGENTS`) | Biggest new surface (coding-agents-as-models). OWUI degrades gracefully (zero agent models when `features.agents` absent/false). Agents would need to run inside the sandbox image with credential/login UX. Separate epic if wanted; keep `features.agents:false` until then |
| 16 | `/notebooks*` | Already deferred; flag stays `false` |
| 17 | Orchestrator lane: `api/v1/terminals/templates*` CRUD + instances + start/stop/restart/reset + `/api/v1/terminals/status` + `X-Terminal-{Template,Chat,Model,Context}-Id` headers + `/openapi.json` per-context discovery | Being selectable as a *Terminals* server unlocks OWUI's template UX (user/chat/model scopes, sharing, admin lifecycle). Strategic decision: we already provide per-chat isolation natively; template management would be a broker feature (chat scope = today's behavior). Large; separate proposal. Plain-connection compatibility does NOT require it |

## Verification

- Pin OWUI **v0.13.4** in `tests/e2e` (currently v0.11.4); add per-flag probes
  (git panel smoke, events live-refresh, processes dock, edit-diff card).
- operationId snapshot test update + a shape-contract test for the new
  `/execute` family (offset arithmetic, `truncated`, WS snapshots).
- WS protocol fixture: resume/cursor replay across reconnect,
  flow-control window (256 KiB), `process` frames under `yes`, chat-rebind
  rejection path.
- `/system` pinned-prompt test refresh (byte-for-byte @ current upstream tag).

## References

- open-terminal `main` @ `94dc004` (v0.17.21), baseline `542094a` (v0.12.3)
- open-webui v0.13.4 (`0666e59`), v0.11.4 (prior baseline)
- open-webui/terminals v0.4.4 (orchestrator; requires OWUI ≥ 0.13.4)
- `docs/compatibility.md` (update matrix + compat tags
  `open-terminal-v0.12.3-skills` → new tag, `owui-v0.11.4` → `owui-v0.13.4`)
