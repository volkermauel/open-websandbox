# open-terminal compatibility

Open Web UI's *Open Terminal* experience is built against the reference
sandbox server [open-terminal](https://github.com/open-webui/open-terminal).
**Version compatibility (v0.4.0):**

| Upstream | Version | Compatibility |
|---|---|---|
| [open-terminal](https://github.com/open-webui/open-terminal) | **v0.17.21** (`94dc004`) | ✅ implemented — the full matrix below (plain single-server contract) |
| [Open Web UI](https://github.com/open-webui/open-webui) | **v0.13.4** | ✅ verified end-to-end (earlier: v0.11.4) |
| [open-webui/terminals](https://github.com/open-webui/terminals) | 0.4.x | ❌ deferred — the template/scope orchestrator API is not implemented; plain Open Terminal connections are unaffected |

The stage-1/2 gap research (v0.12.3 era) is attached to issues #164, #169 and
#195; the stage-3 gap analysis is
`openspec/changes/open-terminal-compat-stage3/`.

Every released image carries two upstream-compatibility tags mirroring this
matrix (stamped by `.github/workflows/release.yml`, bumped per release):

- `owui-v0.13.4` — newest Open Web UI version exercised end-to-end
- `open-terminal-v0.17.21` — open-terminal API surface implemented

This file stays the authoritative matrix; the tags are its machine-readable
pointer for deployers picking a digest.

## Architecture note

Upstream runs one open-terminal server per sandbox with a per-sandbox Bearer
API key. open-websandbox fronts the same client surface through the
**broker**: Open Web UI authenticates once (broker Bearer + `X-User-Id` /
`X-Session-Id` headers), the broker resolves the chat's sandbox and relays
every route listed below to the runtime inside it. File and terminal
endpoints therefore behave per-chat by construction — upstream's
`X-Session-Id` working-directory semantics map to the per-session sandbox
itself.

**Orchestrator note (stage 3):** Open WebUI now also offers
[open-webui/terminals](https://github.com/open-webui/terminals) — a template/
scope orchestrator (user/chat/model environments) that OWUI 0.13.4+ can talk
to instead of a plain open-terminal server. OWUI's changelog is explicit
that **plain Open Terminal connections are not affected**; open-websandbox
remains in that lane (one sandbox per chat is our native scope model). The
Terminals orchestration API (`/api/v1/terminals/templates*`, `X-Terminal-*`
headers) is deliberately NOT implemented — see the stage-3 proposal's
deferred items.

## Endpoint matrix

| Endpoint | Upstream since | Status | Notes |
|---|---|---|---|
| `POST /execute` | 0.1.0 | ✅ | **Background-process model (0.12+)**: log-shape response `{id, command, cwd, status, exit_code, output[], truncated, next_offset}`, `?wait=`/`tail`, `{cwd, env}` body |
| `GET /execute` | 0.12.3 | ✅ | Process list (dock); `WS /execute` = live stream |
| `GET /execute/{id}/status` | 0.12.3 | ✅ | REST + WS (`offset`/`tail`/`wait`); 404 unknown, 4004 on the WS |
| `POST /execute/{id}/input` | 0.12.3 | ✅ | stdin (unicode_escape decode of literal escapes) |
| `DELETE /execute/{id}` | 0.12.3 | ✅ | SIGTERM default, `force` SIGKILL |
| `GET /health` | 0.1.0 | ✅ | Ours is `GET /healthz` |
| `GET /files/cwd` · `POST /files/cwd` | 0.2.7 | ✅ | GET adds `root` for FileNav (#179) |
| `GET /files/list` | 0.2.0 | ✅ | + per-entry `writable` (0.11.35) + `created` birth time (0.17.x, GNU-stat fallback) |
| `GET /files/read` | 0.2.0 | ✅ | `start_line`/`end_line` (0.2.7); images raw; other binaries **415** |
| `POST /files/write` | 0.2.0 | ✅ | + `{created: bool, patch?}` edit-diff card field (0.17.20) |
| `POST /files/replace` | 0.12.3 | ✅ | + `patch?` (0.17.20) |
| `POST /files/mkdir` | 0.2.7 | ✅ | |
| `POST /files/move` | 0.4.2 | ✅ | 409 on existing destination |
| `DELETE /files/delete` | 0.2.7 | ✅ | |
| `GET /files/view` | 0.2.7 | ✅ | Download-oriented (`attachment`); upstream adds `preview=true` PDF render for Office (ours 415-diverges, below) |
| `GET /files/serve/{path}` | 0.11.34 | ✅ | Inline bytes + content type (FileNav iframes) |
| `GET /files/grep` | 0.2.6 | ✅ | |
| `GET /files/glob` | 0.2.6 | ✅ | |
| `GET /files/search` | 0.11.36 | ✅ | Ranked filename picker; gitignore-aware in repos |
| `GET /files/matches` | 0.12.0 | ✅ | Name + content matches, `next_offset` pagination |
| `POST /files/archive` | 0.11.28 | ✅ | ZIP bundle |
| `POST /files/upload` | 0.2.0 | ✅ | Multipart, `directory` param |
| `GET /files/display` | 0.2.9 | ✅ | Show-file signaling: `{path, exists}` |
| `GET /ports` | 0.9.0 | ✅ | Real listing of session-owned listeners |
| `GET/POST/… /proxy/{port}[/{path}]` | 0.9.0 | ✅ | 0.12.2 session-ownership lockdown |
| `POST/GET/DELETE /api/terminals` + WS | 0.7.0 | ✅ | PTY sessions; **stage-3 WS protocol** (below) |
| `GET /api/terminals/user/output` · `POST …/input` | 0.12.3 | ✅ | Active-or-last-used shell (OWUI gates the LLM tools on this probe) |
| `GET /api/config` | 0.8.1 | ✅ | Feature flags: `terminal`/`notebooks`/`system`/`git` (binary probe)/`events`/`agents`/`browser`/`desktop` (sidecar merge) |
| `GET /system` | 0.11.27 | ✅ | Upstream-verbatim prompt **@ v0.17.21** (`display_file` delivery paragraph included) |
| `GET /system/info` | 0.17.x | ✅ | Machine snapshot (2 s cached) — cgroup-scoped cpu/memory, disk, network, top processes |
| `GET /info` | 0.11.6 | ✅ | `{"info": …}`; 404s while `OPEN_TERMINAL_INFO` is unset |
| `WS /events` | 0.17.x | ✅ | `fs:ready`/`fs:change` (fingerprint poll + git markers) + `ports:change`; subscribe validation closes 4000/4003 |
| `GET /git/status` | 0.17.x | ✅ | Porcelain v2 + numstat + untracked line-stats + remote URL |
| `GET /git/diff|log|show|branches|worktrees` | 0.17.x | ✅ | Upstream shapes; bounded runs (60 s → 408, 8 MiB → 413) |
| `POST/PATCH/DELETE /git/{action}` | 0.17.x | ✅ | stage/unstage/discard/commit/checkout/branch×3/worktree/fetch/pull/push/publish/stash/unstash/uncommit |
| `GET/POST /git/pr/*`, `/git/issue/*` | 0.17.x | ✅ | GitHub workbench via `gh`; capabilities degrade `available:false` |
| `/notebooks` | 0.10.0 | ❌ deferred | Reported `false` in `/api/config` |
| `GET /skills` | main post-v0.12.3 | ✅ | Scans `.agents/`, `.cptr/`, `.claude/`, `.codex/skills` under the workspace home; first-name-wins dedup |
| `GET /skills/read?name=` | main post-v0.12.3 | ✅ | `SKILL.md` body + resource walk (depth ≤ 4, ≤ 50 files) |
| `GET /skills/{name}` | — | ✅ | Path-param alias OWUI's backend calls |
| `POST /files/compare` | main post-v0.12.3 | ✅ | Same result shape; **no document extraction** (divergence below) |
| `GET /snapshot` · `PUT /restore` | — | ➕ | Extension of ours (S3 tiering) |
| `/agents` surface | 0.17.19+ | ❌ deferred | Coding-agents-as-models (`features.agents: false` → OWUI offers no agent models) |
| Terminals-orchestrator API | OWUI 0.13.4 / Terminals 0.4.x | ❌ deferred | Plain Open Terminal connections unaffected (see note above) |

## Terminal WebSocket protocol (stage 3)

OWUI 0.13.x's XTerminal opens with the first message
`{"type":"auth","token","chat_id","resume":true,"cursor":N,"flow_control":true,"process_info":true}`
(the broker validates the credential and forwards the fields with the token
stripped — the runtime hop stays per-session-Bearer):

- **resume/cursor** — replay from an absolute byte offset as
  `output{start,end,truncated}` + one binary chunk; below the eviction
  boundary → `truncated:true` (`[Earlier output is no longer available]`).
- **flow_control** — `flow_control` confirm frame; the relay pauses beyond
  the window; `ack{cursor}` releases more.
- **process_info** — `process{pid,name}` frames on foreground change (≤2×/s
  via `/proc/<shell>/stat` field 8 → `/proc/<pgrp>/comm`), driving OWUI tab
  titles.
- **chat{chat_id}** — binds the session (seeded from POST `X-Session-Id`);
  echoed or refused (`Terminal already belongs to another chat`).
- **activity/ping** — tracked / tolerated.
- Close codes: 4004 unknown session, 4009 concurrent attach; legacy clients
  (binary-first or silent) get the pre-stage-3 raw replay.

## Documented divergences

- **Validation status code**: out-of-range/invalid query parameters return
  `400` (axum) where upstream's FastAPI returns `422`. Real clients never
  violate these bounds.
- **No `rg` fast path** in `/files/matches`: content search always uses the
  portable scanner (≤1 MiB files, NUL sniffed, ≤3 matches/file). Same
  results, lower throughput at repo scale.
- **No document extraction** in `/files/read`: upstream converts
  Office/PDF to text via baked-in LibreOffice before 415-ing; we return 415
  directly for non-image binaries. Same divergence applies to
  `/files/compare` (422 instead of an extracted-text diff) and the
  `/files/view?preview=true` Office render.
- **`/files/compare` confinement**: path escapes stay 400 per the repo-wide
  confinement contract.
- **Multi-user OS provisioning** (upstream 0.11.x `sudo -u` per user) is not
  implemented: isolation is per-sandbox (one runtime user per chat). The git
  surface's `sudo -u` branch is likewise absent.
- **Process logs are in memory** (8 MiB ring/process, oldest evicted) vs
  upstream's 50 MiB rotating on-disk JSONL — a sandbox pod's processes are
  ephemeral and the log only feeds the dock/resume windows. The
  `log_path` field upstream includes in the `/execute` responses is
  therefore omitted (no on-disk log exists; OWUI does not read it).
- **No per-session cwd registry**: upstream resolves a no-cwd `/execute`
  against the working directory `POST /files/cwd` set for the session; our
  runtime defaults to `OPEN_TERMINAL_START_DIR` or the workspace base
  (explicit `cwd` in the body works identically). OWUI's git-clone flow
  always sends an explicit cwd; the LLM `run_command` default directory can
  differ from the FileNav selection.
- **`/execute` never auto-kills upstream**; our runtime keeps the
  pre-stage-3 extension: a `timeout` body field kills the group at the
  deadline (`status: killed`, negative-signal exit code). OWUI never sends
  it; the e2e suite does.
- **Terminal flow-control window = scrollback cap** (default 128 KiB) vs
  upstream's 256 KiB window over a larger history — the unacked bytes always
  remain in our ring, so steady-state playback never truncates. Terminal
  WS input validation is also more permissive than upstream: a resume
  `cursor` beyond the history end clamps to it (upstream closes 4000
  "Invalid output cursor") and `ack{cursor}` accepts any u64 — self-harm
  only, never a server failure.
- **`/events` is a bounded-recursive fingerprint poll** (1 s; every path's
  mtime + file sizes under the watched directory, `.git` trees skipped,
  `.git/HEAD` + the RESOLVED branch-ref + `index`/`ORIG_HEAD` markers so
  commits fire) vs upstream's native recursive watchfiles watcher. Same wire
  semantics (no filenames, `fs:ready`/`fs:change`, empty subscribe =
  unsubscribe); trees beyond 10 000 entries degrade to a bounded prefix —
  no inotify dependency under gVisor.
- **Python sentence in `/system`**: upstream's *"Python {version} is
  available."* renders only when a python3 probe succeeds (the workbench
  toolchain image ships python3; the prompt is byte-for-byte upstream there).
- **`/info` conditional**: upstream registers the route only when
  `OPEN_TERMINAL_INFO` is set; we mirror with a 404 `{"detail":"Not Found"}`
  (same wire behavior).
- **`/files/display` confinement**: ours resolves the path inside the
  workspace (400 on escape) like every other file endpoint; upstream's
  single-user mode resolves arbitrary host paths.
- **`/proxy` transport details**: upstream strips `content-encoding` from
  forwarded responses because httpx already decoded the body; our client
  passes bytes through uncompressed, so we keep the header — self-consistent
  either way.
- **`/system` toolchain append** (workbench image): when
  `SANDBOX_TOOLS_MANIFEST` is set, `GET /system` appends two sections after
  the upstream-verbatim prompt. Append-only; with the knob off the response
  is byte-for-byte upstream.
- **`/system/info` `setup` key**: present only when
  `OPEN_TERMINAL_SETUP_SCRIPT` is configured; the script runs at boot (the
  log tail comes from captured output, upstream keeps
  `~/.open-terminal/setup.log`).
- **`environment: container`**: set the `container` env var on the runtime
  container (chart default) so the modal reports `container` rather than
  `host` — upstream infers it from Docker markers that gVisor pods lack.

## 0.17.7 workspace knobs

`OPEN_TERMINAL_START_DIR` (default cwd for new shells, terminals and the
browser cwd; created at boot, confined to the workspace) and
`OPEN_TERMINAL_SETUP_SCRIPT` (runs once in the start dir before serving;
outcome via `/system/info`) are implemented with upstream semantics.

## Desktop / browser lane

The desktop/browser surfaces ride the sidecar image
(`ghcr.io/volkermauel/open-terminal:<version>-desktop-sandbox`, currently
pinned at 0.16.1): `GET /desktop`, `WS /desktop/stream` (frames/VNC),
`WS /desktop/audio`, `GET|POST /browser/sessions*`, `WS /browser/view/*`,
plus the `/desktop` capability fields OWUI 0.13 reads (`transport`, `audio`,
`clipboard`). The 0.17.x desktop line (audio hardening, window-drag/input
ordering, XFCE-default) is a recommended sidecar image bump — a chart-value
change once the fork image is rebuilt; no runtime code depends on it.
