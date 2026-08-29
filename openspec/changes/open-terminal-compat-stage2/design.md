# Design: open-terminal compatibility stage 2

Upstream contracts read from the reference server at v0.12.3
(`open_terminal/main.py`, `open_terminal/env.py`,
`open_terminal/utils/port.py`; clone `/tmp/ott`). Line references below are
against that checkout.

## `GET /system` (upstream main.py:461-471, get_system_prompt main.py:96-118)

Auth-protected (`Authed` ≙ upstream `verify_api_key`). Response:
`{"prompt": "<text>"}`.

The prompt renderer is a pure function over a `Grounding` struct so the
verbatim text is unit-pinned with synthetic values:

- Default prompt — copied **byte-for-byte** from upstream
  `get_system_prompt()` (see `rust/runtime/src/system.rs`; provenance comment:
  *"system prompt upstream-verbatim @ v0.12.3 (open_terminal/main.py)"*):
  first paragraph grounds OS/kernel/arch/hostname/user/shell/python in one
  line, `\n\n`, then the single-paragraph tool-usage text with the U+2014 em
  dash preserved. When `OPEN_TERMINAL_INFO` is set, `\n\n{info}` is appended
  (upstream parity).
- Template expansion (0.11.35): when `OPEN_TERMINAL_SYSTEM_PROMPT` is set, the
  default is replaced by that template with `{{\s*([a-zA-Z0-9_]+)\s*}}`
  substituted from the grounding map (keys: `os`, `kernel`, `arch`,
  `hostname`, `user`, `shell`, `python_version`, `home`); unknown keys pass
  through verbatim. Same regex, same key set, same fallback behavior as
  upstream `_expand_system_prompt_template` (main.py:87-104).
- Grounding values: `uname()` for os/kernel/arch/hostname (nix `utsname`
  feature); `user` = `$USER` else `"unknown"`; `shell` = the runtime's
  effective shell (`$SHELL`, else our `/bin/bash` default — the shell
  `/execute` actually runs; upstream defaults to `/bin/sh` when its env is
  unset, a value-only difference); `home` = `$HOME` else the passwd entry of
  the current uid (upstream `os.path.expanduser("~")`); `python_version` =
  cached `python3 --version` / `python --version` probe output.
- **Sentence-level divergence (documented)**: upstream's *"Python
  {python_version} is available."* is rendered only when the probe finds a
  Python. Our default image has none (debian bookworm-slim + libreoffice-nogui
  — verified via the Debian Packages index that no python3 enters the tree),
  and a system prompt must not assert an interpreter the model cannot invoke.
  With python3 present (user-installed, or a future image) the rendered prompt
  is byte-for-byte upstream.

## `GET /info` (upstream main.py:473-483)

Upstream registers the route only `if OPEN_TERMINAL_INFO:` — with the env var
unset the path 404s (`{"detail":"Not Found"}`). We mirror: `RuntimeConfig.info`
(env `OPEN_TERMINAL_INFO`, default empty) — empty ⇒ 404 `Not Found`, set ⇒
`{"info": "<value>"}`. Auth-protected like upstream.

## `GET /files/display` (upstream main.py:629-663)

Upstream contract: query `path` (required), resolve against the session cwd
(our per-session sandbox ⇒ the workspace base via `safe_path`), then
`{"path": <resolved absolute>, "exists": isfile}` — **no file bytes are
served**; the endpoint signals the client to open its own viewer. A missing
file is a *successful* response with `exists: false` (only an invalid path
errors). Divergence: ours confines the resolved path to the workspace
(`safe_path` ⇒ 400 on escape) like every other file endpoint — upstream's
single-user mode allows arbitrary absolute host paths; our runtime's contract
is workspace confinement.

## `/proxy/{port}[/{*path}]` (upstream main.py:1809-1873, utils/port.py)

- **Route**: upstream `/proxy/{port}/{path:path}` for GET/POST/PUT/PATCH/
  DELETE/HEAD/OPTIONS. Ours registers both `/proxy/{port}` and
  `/proxy/{port}/{*path}` with exactly those seven methods (axum); other
  methods 405 like upstream.
- **Port validation**: `1..=65535` else the upstream message `"Port must be
  between 1 and 65535"` — upstream 422, ours 400 (the already-documented
  FastAPI-vs-axum validation-status divergence).
- **Ownership (0.12.2 lockdown)**: upstream allows a port iff it appears in
  `_visible_ports(request)` — single-user mode: listening sockets owned by
  *descendant processes of the server*. We implement exactly that
  (`rust/runtime/src/ports.rs`): parse `/proc/net/tcp` + `/proc/net/tcp6` for
  `LISTEN` (state `0A`) rows, resolve socket inode → pid by scanning
  `/proc/<pid>/fd/*` readlinks (`socket:[inode]`), build the process tree from
  `/proc/<pid>/stat` (robust parse: everything after the final `)`, ppid is
  field 2 of the remainder — upstream's whitespace split breaks on comms with
  spaces; ours is a strict improvement), and keep sockets whose pid is a
  descendant of the runtime pid (the runtime's own :8888 is therefore
  excluded, matching upstream's exclusive-descendants rule). `/execute`
  children and PTY shells are runtime children ⇒ services the model starts
  are owned; a reparented orphan becomes PID 1's child — PID 1 in the sandbox
  *is* the runtime ⇒ still owned. Unowned ⇒ 404 `"Port not found"`. The
  multi-user UID branch does not apply (single OS user per sandbox —
  documented divergence).
- **Forwarding**: `http://localhost:{port}/{path}` + the inbound query string;
  request headers minus `host`, `transfer-encoding`, `connection`,
  `authorization` (upstream strips the API key — the proxied app never sees
  it; `content-length` is dropped and recomputed). Body buffered and forwarded
  (upstream `await request.body()` — same). Client: one shared `reqwest`
  client in `AppState` (`default-features = false`, no TLS — localhost HTTP
  only), 300 s total / 5 s connect timeout, redirects off — upstream
  `httpx.AsyncClient(timeout=Timeout(300.0, connect=5.0),
  follow_redirects=False)`.
- **Upstream errors**: transport failure ⇒ 502 `"Connection refused:
  localhost:{port}"`; timeout ⇒ 504 `"Timeout connecting to localhost:{port}"`
  (both exact upstream strings).
- **Response**: upstream status + headers minus `transfer-encoding`,
  `connection`, `content-length`. Upstream also strips `content-encoding`
  because httpx already *decoded* the body; our client does not decompress, so
  we **keep** `content-encoding` — the bytes and headers stay self-consistent
  (delivery-equivalent, documented micro-divergence; ours does not advertise
  `Accept-Encoding` so identity encoding is the norm anyway).

## `/ports` upgrade (upstream main.py:1768-1795)

Same visibility function feeds `/ports` upstream (`_visible_ports`), so our
stub (`{"ports": []}`) becomes the real listing: `{port, pid, process}` per
row (uid stripped exactly like upstream; `pid`/`process` null when the owning
process died mid-scan). Sorted by port.

## `/api/config` flip

Runtime `api_config` and broker `Features` both flip `system: false → true`;
broker parity test + OpenAPI snapshot updated. e2e assertion flips with them.

## Testing

- **Unit** (`system.rs`): the default prompt pinned byte-exact against the
  upstream literal with synthetic grounding; template expansion (known key,
  spacing variants `{{os}}`/`{{ os }}`, unknown key passes through); info
  appended when set; python sentence omitted when the probe fails.
- **Contract** (`tests/open_terminal_compat_stage2.rs`): /system auth + shape
  + host grounding; /info 404-when-unset + 200-when-set (new
  `Env::with_config` harness hook); /files/display exists/missing/traversal/
  no-param; /ports shape; /proxy happy path against a **real spawned runtime
  binary child** (`CARGO_BIN_EXE_runtime` binds :8888; the child is a true
  descendant ⇒ proxy reaches it, and proxying its `/files/list` yields the
  child's 401 — proving `Authorization` is stripped); unowned-port 404 with
  the exact `"Port not found"` body using an in-process listener (owned by
  the runtime process itself ⇒ *not* a descendant ⇒ invisible, exactly the
  upstream lockdown); port-range 400 with the upstream message.
- **e2e** (`tests/e2e/test_open_terminal_compat.py`): through the broker
  relay — /system 200 + grounded prefix, /info 404 (chart sets no
  `OPEN_TERMINAL_INFO`), /files/display round-trip, /ports shape,
  /proxy/0 400, unowned port 404 `"Port not found"`, and the runtime's own
  :8888 **not** proxyable (404) — the ownership lockdown proven live. The
  happy path through the relay needs a listener inside the sandbox image,
  which ships none (no python/nc/socat); it is covered by the contract tests
  against the real binary instead — documented.
- **Gates**: fmt/clippy/`cargo test --workspace`, `openspec validate`,
  `mkdocs build --strict`, broker OpenAPI snapshot regen
  (`REGEN_SNAPSHOT=1`).

## Wire compatibility notes

Additive except: `features.system` flips to `true` (truthful now), `/ports`
returns real data (upstream parity; clients already handle object rows —
upstream shape). No existing request/response shape changes.
