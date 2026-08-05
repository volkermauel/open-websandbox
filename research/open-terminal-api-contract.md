# Open Terminal — API Contract Specification

**Purpose:** Precise HTTP request/response shapes for every endpoint the Open WebUI
terminal UI calls, derived from reading the source of:

- **Backend (authoritative shapes):** `open-terminal/open_terminal/main.py` (+ `env.py`,
  `utils/fs.py`, `utils/log.py`, `utils/port.py`)
- **k8s proxy:** `open-terminal-k8s-proxy/terminal_proxy/main.py` (+ `proxy/http.py`,
  `chat_bootstrap.py`, `pod_manager.py`)
- **UI client (for the init handshake):** `open-webui/src/lib/apis/terminal/index.ts`,
  `open-webui/src/lib/components/chat/FileNav.svelte`,
  `open-webui/src/lib/components/AddTerminalServerModal.svelte`

This is the contract our `server.py` runtime + broker must reproduce so the Open WebUI
terminal UI connects unchanged.

---

## 0. Cross-cutting facts (read first)

### Authentication

- **Mechanism:** Bearer token via `Authorization: Bearer <KEY>`.
- **Backend `verify_api_key`:** (`main.py:124`) uses `HTTPBearer(auto_error=False)`.
  If `OPEN_TERMINAL_API_KEY` is unset → auth is a no-op (returns `None`). If set, the
  bearer credential must `hmac.compare_digest`-match → else `401 {"detail": "Invalid API key"}`.
- **Proxy `verify_api_key`:** (`proxy/main.py:95`) compares against a generated/loaded
  `PROXY_API_KEY`. Returns the credential string; raises `401` on mismatch. If
  `PROXY_API_KEY` is empty it returns `"anonymous"`.

### Identity / scoping headers

- **`X-User-Id`** — backend reads it **optionally** (`main.py:142`, only used in
  `MULTI_USER` mode to pick an OS user). The **proxy REQUIRES** it for every *proxied*
  endpoint via `extract_user_id` (`proxy/main.py:106`) → `400 {"detail": "X-User-Id header required"}`
  if missing. The proxy's *static* endpoints (`/api/config`, `/api/status`, `/health`,
  `/metrics`) do **not** require it.
- **`X-Session-Id`** — read **optionally** by both backend and proxy (`extract_chat_id`).
  It scopes the **per-chat working directory** (in-memory `_session_cwds` map, 7-day TTL).
  The frontend sends the Open WebUI `chatId` here. When the proxy also runs per-chat-cwd
  bootstrap (`chat_bootstrap.py`), it uses `X-Session-Id` to `mkdir + POST /files/cwd`
  a private chat dir on the pod before the PTY spawns.

### Proxy header handling (`proxy/http.py`)

- The proxy **strips** `host, content-length, transfer-encoding, connection, authorization`
  from the inbound request and **replaces** `Authorization` with `Bearer <pod_api_key>`.
- All other headers (`X-User-Id`, `X-Session-Id`, query string, body) are forwarded.
- Response headers `content-encoding, content-length, transfer-encoding, connection` are
  stripped. Binary/streaming content types (`application/octet-stream`, `image/`,
  `application/pdf`, `video/`, `audio/`) are streamed.

### Other middlewares (backend)

- `normalize_null_query_params` (`main.py:187`): strips any query param whose value is the
  literal string `"null"` (case-insensitive). So `?directory=null` behaves like omitting it.
- `permission_error_handler` (`main.py:182`): `PermissionError` → `403 {"detail": "<msg>"}`.

### Proxy-wide failure envelopes (not backend)

These only apply when going *through the proxy*; our broker may emit them too:

- K8s unavailable → `503 {"error": "Service temporarily unavailable", "detail": "Kubernetes API is unavailable"}`
- Rate limit (300 req/min/IP) → `429 {"error": "Rate limit exceeded", "detail": "Too many requests"}`
- Body > 100 MiB → `413 {"error": "Payload too large", "detail": "Maximum size is 104857600 bytes"}`
- Pod connect error / timeout → `504 {"error": "Terminal pod timeout"}` / `503 {"error": "Terminal pod unavailable"}`
- Circuit breaker open → `503 {"error": "Circuit breaker open", "detail": "..."}`

---

## 1. Init / Config

### `GET /api/config`

Feature discovery. This is the endpoint the UI's **connection test** calls — the trigger
for "Server connection failed" (`AddTerminalServerModal.svelte:166`).

**Auth:** Backend — **none** (no `dependencies` on the route, `main.py:408`).
Proxy — `verify_api_key` required, but **no `X-User-Id`** (`proxy/main.py:342`).

> ⚠️ **The static shape differs between the two implementations.** Match whichever layer
> your broker impersonates. The UI only reads `features.terminal` (`FileNav.svelte:283`,
> `terminal/index.ts:14`), so both shapes pass the UI, but be deliberate.

**Backend response** (`main.py:414`) — served statically by the backend itself:

```json
{
  "features": {
    "terminal": true,
    "notebooks": true,
    "system": true
  }
}
```

All three are **booleans** from env: `ENABLE_TERMINAL`, `ENABLE_NOTEBOOKS`, `ENABLE_SYSTEM_PROMPT`.

**Proxy response** (`proxy/main.py:347`) — served **statically by the proxy** (never proxied):

```json
{
  "features": {
    "terminal": true,
    "notebooks": true,
    "desktop": false
  }
}
```

Note the proxy advertises `desktop` (always `false`) and omits `system`.

**Status:** `200`. **Body/query:** none.

---

### `GET /api/v1/policies`
>
> ❌ **This endpoint does not exist** in either the backend or the proxy. The only
> `policies` references in the repos are Kubernetes `NetworkPolicy` (k8s networking
> egress rules) in `open-terminal-k8s-proxy/openspec/changes/user-based-network-policy/`
> and the README — they are not HTTP endpoints. Do **not** implement it unless a different
> upstream component (e.g. an Open WebUI core route) is the real source. As far as the
> terminal backend/proxy contract goes, it is absent.

---

### `GET /api/status`
>
> ⚠️ **Proxy-only, served statically.** There is **no** `/api/status` in the backend.

**Auth:** `verify_api_key` (no `X-User-Id`). **Body/query:** none.
**Source:** `proxy/main.py:358` → returns `pod_manager.get_stats()` (`pod_manager.py:468`).

**Response (`200`):**

```json
{
  "active_pods": 0,
  "max_pods": 10,
  "pods": [
    {
      "user_hash": "a1b2c3...",
      "pod_name": "terminal-a1b2c3...",
      "state": "running",
      "last_active": "2025-01-01T12:00:00.000000+00:00"
    }
  ]
}
```

`state` is the `PodState` enum value; `pods` is `[]` when idle. This is an operator/telemetry
endpoint, **not** called by the terminal UI init handshake.

---

### `GET /info`

Operator-provided environment description surfaced to the AI.

**Backend:** registered **only if `OPEN_TERMINAL_INFO` is truthy** (`main.py:435`). Returns:

```json
{ "info": "<OPEN_TERMINAL_INFO string>" }
```

**Auth:** `verify_api_key`. **Status:** `200`. **Proxy:** always registers `/info` and
**proxies** it to the pod (`proxy/main.py:380`) — requires `X-User-Id`. If the pod has no
`/info` (backend didn't register it) the proxied call returns `404`.

---

### `GET /ports`

List TCP ports listening on localhost (spawned by the terminal session).

**Auth:** backend `verify_api_key` (`main.py:1337`). **Proxy:** proxied (`proxy/main.py:689`),
requires `X-User-Id`.
**Query params:** none. **Body:** none.

**Backend scoping:** single-user → ports owned by descendant processes of the server PID;
multi-user → ports owned by the `X-User-Id` user's UID. On a restricted runtime where user
provisioning fails it returns `{"ports": []}` (`main.py:1355`).

**Response (`200`)** (`main.py:1374`, `utils/port.py`):

```json
{
  "ports": [
    { "port": 3000, "pid": 1234, "process": "node" },
    { "port": 8080, "pid": 2345, "process": "python3" }
  ]
}
```

Each entry has exactly `port` (int), `pid` (int|null), `process` (str|null). The internal
`uid` field is **popped** before returning (`main.py:1372`). List is sorted by `port`.
UI type (`terminal/index.ts:8`): `{ port: number; pid: number|null; process: string|null }`.

---

## 2. Filesystem

### `POST /files/cwd` — set session cwd

**Body** (`MkdirRequest`, reused, `main.py:477`):

```json
{ "path": "/home/user/projects/foo" }
```

**Auth:** backend `verify_api_key`; reads optional `X-Session-Id` and stores it in
`_session_cwds`. Proxy: `verify_api_key` + **`X-User-Id` required**, passes `X-Session-Id` through.

**Behavior:** resolves `path`; if not multi-user and the dir doesn't exist → `404 {"detail":"Directory not found"}`. Stores the resolved path as this session's cwd.

**Response (`200`)** (`main.py:488`):

```json
{ "cwd": "/home/user/projects/foo" }
```

UI (`setCwd`): expects `{ cwd: string }`.

---

### `GET /files/cwd` — get session cwd

**Auth:** backend `verify_api_key`; reads `X-Session-Id`. Proxy: `verify_api_key` + `X-User-Id`.
**Query/body:** none.

**Response (`200`)** (`main.py:462`):

```json
{
  "cwd": "/home/user/projects/foo",
  "home": "/home/user",
  "root": { "path": "/home/user", "label": "Home" }
}
```

- `cwd` = the session-scoped cwd (or `home` default when no session tracked).
- `home` always present.
- `root` is **conditional** (`main.py:467`): included only when `FILE_BROWSER_ROOT`
  (`OPEN_TERMINAL_FILE_BROWSER_ROOT`, default `"home"`) is not `"filesystem"`. When
  `"filesystem"`, `root` is **omitted** (meaning "browse whole FS"). `root` is
  `{ "path": str, "label": str }`. UI `TerminalFileRoot`/`TerminalCwd` types confirm:
  `cwd` (nullable), optional `home`, optional `root`.

**Init note:** `getCwd` sends `X-Session-Id: <chatId>` when a chat is open
(`terminal/index.ts:70`).

---

### `GET /files/list` — list directory

**Query params:** `directory` (str, default `"."`). Other params are ignored.
**Auth:** backend `verify_api_key`; reads `X-Session-Id` to resolve relative `directory`.
Proxy: `verify_api_key` + `X-User-Id`.

**Behavior:** resolves `directory` against session cwd; if not a dir → `404 {"detail":"Directory not found"}`.

**Response (`200`)** (`main.py:513`, entry shape from `fs.py:184` `listdir`):

```json
{
  "dir": "/home/user/projects/foo",
  "entries": [
    { "name": "src",      "type": "directory", "size": 4096,  "modified": 1718000000.0 },
    { "name": "README.md","type": "file",      "size": 1234,  "modified": 1718000000.0 }
  ]
}
```

The top-level key is **`dir`** (the resolved path) and **`entries`** (array).
Each entry has exactly four keys — **`name`** (str), **`type`** (`"directory"`|`"file"`),
**`size`** (int bytes), **`modified`** (float, `st_mtime` epoch seconds). Entries are
sorted by name (`os.listdir` + `sorted`). UI `FileEntry` type confirms
(`terminal/index.ts:1`) — and the UI reads `res.entries` (`index.ts:101`).

**Init note:** `listFiles` sends `?directory=<encoded path>` and optional `X-Session-Id`.

---

### `GET /files/read` — read a file
>
> Note: only **GET** is implemented in the backend. There is **no** `POST /files/read`.
> The proxy's catch-all `/files/{path:path}` would forward a `POST /files/read`, but the
> backend would return `405 Method Not Allowed`. The UI only uses GET.

**Query params:** `path` (str, required), `start_line` (int≥1, optional), `end_line` (int≥1, optional).
**Auth:** backend `verify_api_key`; reads `X-Session-Id`. Proxy: `verify_api_key` + `X-User-Id`.

**Response — text file (`200`)** (`main.py:584`):

```json
{
  "path": "/home/user/README.md",
  "total_lines": 42,
  "content": "# Title\n...full text of requested line range..."
}
```

**Response — image / allowed binary MIME** (`200`): raw bytes with the guessed
`media_type` (e.g. `image/png`) via `Response(content=raw, media_type=mime)`.
Allowed prefixes come from `OPEN_TERMINAL_BINARY_MIME_PREFIXES` (default `"image"`).
**Response — document (PDF/Office/ODF)** (`200`): same JSON shape as text, after
text-extraction. **Response — other binary** `415 {"detail": "Unsupported binary file type: <mime> (<n> bytes)"}`.
**404** if not a file: `{"detail": "File not found"}`.

UI (`readFile`) inspects `Content-Type`: if it starts with `image/` or `application/octet`
it treats the body as binary; otherwise parses JSON and reads `.content`.

---

### `POST /files/write` — write text to a file

**Body** (`WriteRequest`, `main.py:666`):

```json
{ "path": "/home/user/notes.txt", "content": "hello world\n" }
```

**Auth:** backend `verify_api_key`; reads `X-Session-Id` for relative path resolution.
Proxy: `verify_api_key` + `X-User-Id`. Creates parent dirs; overwrites if exists.

**Response (`200`)** (`main.py:674`):

```json
{ "path": "/home/user/notes.txt", "size": 12 }
```

`size` is byte length of the UTF-8 encoded content. `400 {"detail": "<err>"}` on OS/subprocess error.

---

### `POST /files/replace` — find/replace in a file

**Body** (`ReplaceRequest`, `main.py:749`):

```json
{
  "path": "/home/user/app.py",
  "replacements": [
    { "target": "old_value", "replacement": "new_value", "allow_multiple": false },
    { "target": "debug=", "replacement": "DEBUG=", "start_line": 1, "end_line": 50, "allow_multiple": true }
  ]
}
```

`ReplacementChunk` fields: `target` (str, required), `replacement` (str, required),
`start_line` (int≥1, optional), `end_line` (int≥1, optional), `allow_multiple` (bool, default `false`).

**Auth:** backend `verify_api_key`; reads `X-Session-Id`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:794`):

```json
{ "path": "/home/user/app.py", "size": 2048 }
```

Errors: `404 {"detail":"File not found"}`; `400` for target-not-found
(`"Target string not found: ..."`) or ambiguous match (`"Found N occurrences ... allow_multiple is false"`).

---

### `POST /files/mkdir` — make directory

**Body** (`MkdirRequest`):

```json
{ "path": "/home/user/newdir/sub" }
```

**Auth:** backend `verify_api_key` (no session header used). Proxy: `verify_api_key` + `X-User-Id`.
Creates parents.

**Response (`200`)** (`main.py:688`):

```json
{ "path": "/home/user/newdir/sub" }
```

`400 {"detail": "<err>"}` on failure.

---

### `POST /files/move` — move/rename

**Body** (`MoveRequest`, `main.py:716`):

```json
{ "source": "/home/user/a.txt", "destination": "/home/user/b.txt" }
```

**Auth:** backend `verify_api_key`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:734`):

```json
{ "source": "/home/user/a.txt", "destination": "/home/user/b.txt" }
```

`404 {"detail":"Source path not found"}`; `400 {"detail":"Destination parent directory not found"}`;
`409 {"detail":"Destination already exists"}`.

---

### `POST /files/delete` / `DELETE /files/delete` — delete entry
>
> ⚠️ **Method is `DELETE`, not `POST`.** Backend registers `@app.delete("/files/delete")`
> (`main.py:691`). The UI's `deleteEntry` uses `method: 'DELETE'` with `?path=...`
> (`terminal/index.ts:233`). A `POST` to `/files/delete` is **not** a defined handler;
> the proxy catch-all would forward it but the backend returns `405`.

**Query params:** `path` (str, required). **No body.**
**Auth:** backend `verify_api_key`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:708`):

```json
{ "path": "/home/user/old", "type": "directory" }
```

`type` is `"directory"` or `"file"`. `404 {"detail":"Path not found"}`; `400` on failure.

---

### `GET /files/grep` — content search

**Query params:** `query` (str, req), `path` (str, default `"."`), `regex` (bool, default `true`),
`case_insensitive` (bool, default `false`), `include` (list[str], optional — repeatable glob),
`match_per_line` (bool, default `true`), `max_results` (int, 1–500, default `50`).
**Auth:** backend `verify_api_key`; reads `X-Session-Id`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:903`):

```json
{
  "query": "TODO",
  "path": "/home/user/src",
  "matches": [
    { "file": "/home/user/src/app.py", "line": 12, "content": "# TODO: refactor this" }
  ],
  "truncated": false
}
```

Match shape depends on `match_per_line`:

- `true` (default): `{"file": str, "line": int, "content": str}`
- `false` (files-with-matches): `{"file": str}`

`truncated` is `true` when `max_results` was hit. `404 {"detail":"Search path not found"}`;
`400 {"detail":"Invalid regex: ..."}`.

---

### `GET /files/glob` — name search

**Query params:** `pattern` (str, req, e.g. `"*.py"`), `path` (str, default `"."`),
`exclude` (list[str], optional), `type` (str, default `"any"`, one of `file|directory|any`),
`max_results` (int, 1–500, default `50`).
**Auth:** backend `verify_api_key`; reads `X-Session-Id`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:1005`):

```json
{
  "pattern": "*.py",
  "path": "/home/user/src",
  "matches": [
    { "path": "app.py", "type": "file", "size": 1234, "modified": 1718000000.0 },
    { "path": "utils/helpers.py", "type": "file", "size": 567, "modified": 1718000000.0 }
  ],
  "truncated": false
}
```

Each match: `path` (**relative** to the search dir), `type` (`"file"`|`"directory"`),
`size` (int), `modified` (float `st_mtime`). `404 {"detail":"Search directory not found"}`.

---

### `POST /files/upload` — multipart upload

**Content-Type:** `multipart/form-data`.
**Form/query:** `directory` (query str, required), `file` (form file field, required).
**Auth:** backend `verify_api_key`. Proxy: catch-all proxied (`verify_api_key` + `X-User-Id`).

**Response (`200`)** (`main.py:1046`):

```json
{ "path": "/home/user/uploads/photo.png", "size": 98765 }
```

Filename is `os.path.basename(file.filename or "upload")`. `403` on PermissionError,
`400` on OSError. UI (`uploadToTerminal`) posts `FormData` with a single `file` field.

---

### `POST /files/archive` — ZIP multiple paths

**Body** (`ArchiveRequest`, `main.py:1061`):

```json
{ "paths": ["/home/user/projects/foo", "/home/user/notes.txt"] }
```

**Auth:** backend `verify_api_key`. Proxy: catch-all proxied (`verify_api_key` + `X-User-Id`).

**Response (`200`)** — **binary, not JSON** (`main.py:1109`):

- `Content-Type: application/zip`
- `Content-Disposition: attachment; filename="<archive_name>.zip"`
  (`archive_name` = single path basename, or `"download"` for multiple)

`400 {"detail":"No paths provided"}`; `404 {"detail":"Path not found: <p>"}`.

---

## 3. Execute (background shell commands)

> **`output` entry shape** (used by `/execute`, `/execute/{id}/status`) comes from the
> JSONL process log (`utils/log.py:255`). Each entry is:
>
> ```json
> { "type": "stdout", "data": "hello\n" }
> ```
>
> `type` ∈ `{"stdout", "stderr", "output"}`, `data` is a string. The `start`/`end`/`log_rotated`
> marker records are NOT included in `output`.

### `POST /execute` — run a command

**Body** (`ExecRequest`, `main.py:1157`):

```json
{
  "command": "echo hello && ls -la",
  "cwd": "/home/user/projects/foo",
  "env": { "FOO": "bar" }
}
```

`command` (str, req); `cwd` (str, opt — resolved against session cwd or defaults to home);
`env` (dict[str,str], opt — merged onto `os.environ`).

**Query params:** `wait` (float 0–300, opt — seconds to block for completion; if unset and
`OPEN_TERMINAL_EXECUTE_TIMEOUT` is set, that becomes the default), `tail` (int≥1, opt — last N entries).

**Auth:** backend `verify_api_key`; reads `X-Session-Id` for cwd scoping. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:1204`):

```json
{
  "id": "20250722-143012-a1b2c3",
  "command": "echo hello && ls -la",
  "status": "running",
  "exit_code": null,
  "output": [
    { "type": "stdout", "data": "hello\n" }
  ],
  "truncated": false,
  "next_offset": 1,
  "log_path": "/home/user/.local/state/open-terminal/logs/processes/20250722-143012-a1b2c3.jsonl"
}
```

- `id`: `time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:6]`.
- `status`: `"running"` initially, becomes `"done"` or `"killed"`.
- `exit_code`: `null` while running, int on completion.
- `output`: array of `{type, data}` (see note above).
- `next_offset`: int — pass as the next poll's `offset`.
- `truncated`: bool — whether more output existed than returned.
- `log_path`: server-side file path (exposed; harmless).

---

### `GET /execute/{process_id}/status` — poll output / status

**Path:** `process_id` (the id from `POST /execute`).
**Query params:** `wait` (float 0–300, opt), `offset` (int≥0, default `0` — pass `next_offset`),
`tail` (int≥1, opt).
**Auth:** backend `verify_api_key`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** — **identical shape to `POST /execute`** (`main.py:1262`). Output is
drained relative to `offset` to keep memory bounded.

**404** if process unknown/expired: `{"detail":"Process not found"}` (processes auto-expire
5 min after finishing).

---

### `POST /execute/{process_id}/input` — send stdin

**Body** (`InputRequest`, `main.py:1286`):

```json
{ "input": "yes\n" }
```

`input` (str, req). Literal escape sequences (`\n`, `\x03` for Ctrl-C) are decoded to real bytes.

**Auth:** backend `verify_api_key`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:1302`): `{ "status": "ok" }`.
`404 {"detail":"Process not found"}`; `400 {"detail":"Process has already exited"}` or
`{"detail":"Process stdin is closed"}`.

---

### `DELETE /execute/{process_id}` — kill command

**Query params:** `force` (bool, default `false` — `SIGKILL` instead of `SIGTERM`).
**Auth:** backend `verify_api_key`. Proxy: `verify_api_key` + `X-User-Id`.

**Response (`200`)** (`main.py:1328`): `{ "status": "killed" }`.
**404** `{"detail":"Process not found"}`. (Also exists: `GET /execute` → list of
`{id, command, status, exit_code, log_path}` — not in the required set but implemented at `main.py:1133`.)

---

## 4. Terminals (interactive PTY sessions; WS protocol out of scope)

All four require backend `verify_api_key`. Through the proxy they require
`verify_api_key` + **`X-User-Id`** and `POST` triggers per-chat-cwd bootstrap
(`force_chat_dir=True`). They are only registered when `ENABLE_TERMINAL` is true
(wrapped in `if ENABLE_TERMINAL:` block, `main.py:1450`).

### `POST /api/terminals` — create session

**Body:** none. **Auth:** as above.

**Response (`200`)** (`main.py:1620`):

```json
{ "id": "a1b2c3d4", "created_at": "2025-07-22T14:30:12.123456Z", "pid": 12345 }
```

- `id`: `str(uuid.uuid4())[:8]` (8 hex chars). **Note:** if the request carries
  `X-Session-Id`, the backend *reuses it as the terminal session id* (`main.py:1559`) so the
  PTY cwd can be seeded from the per-chat cwd. So the returned `id` may equal the chat id.
- `created_at`: ISO-8601 UTC with trailing `Z` (`datetime.utcnow().isoformat() + "Z"`).
- `pid`: OS process id of the spawned shell.

**Error envelopes (JSONResponse, not FastAPI default):**

- `503 {"error": "PTY not available on this platform (install pywinpty on Windows)"}`
- `429 {"error": "Maximum number of terminal sessions (16) reached"}` (limit = `MAX_TERMINAL_SESSIONS`, default 16)
- `503 {"error": "Out of PTY devices — too many active terminals or processes"}`

---

### `GET /api/terminals` — list sessions

**Body:** none. **Response (`200`)** (`main.py:1635`) — a bare **array** (not wrapped):

```json
[
  { "id": "a1b2c3d4", "created_at": "2025-07-22T14:30:12.123456Z", "pid": 12345 }
]
```

Dead sessions are pruned before returning.

---

### `GET /api/terminals/{session_id}` — get one session

**Response (`200`)** (`main.py:1663`): `{ "id": str, "created_at": str, "pid": int }`.
**404** (`JSONResponse`): `{"error":"Session not found"}` (also returned if the process died).

---

### `DELETE /api/terminals/{session_id}` — kill session

**Response (`200`)** (`main.py:1676`): `{ "status": "deleted" }`.
**404** (`JSONResponse`): `{"error":"Session not found"}`.

---

## 5. Static vs proxied (per the k8s proxy)

The proxy serves these **statically** (it computes the response itself; our broker must too):

| Endpoint | Static response |
|---|---|
| `GET /api/config` | `{features:{terminal:true,notebooks:true,desktop:false}}` |
| `GET /api/status` | `pod_manager.get_stats()` (proxy-only; no backend equivalent) |
| `GET /health` | `{status, k8s, active_pods, max_pods, storage_mode}` |
| `GET /metrics` | Prometheus text |

**Everything else is proxied** to the per-user/per-chat terminal pod, with `Authorization`
rewritten to `Bearer <pod_api_key>` and `X-User-Id` required:
`/info`, `/system`, `/files/*`, `/execute*`, `/ports`, `/proxy/{port}/{path}`,
`/api/terminals*`, `/notebooks*`, `/desktop*`.

The proxy has explicit per-endpoint handlers for the *named* routes above plus catch-alls:

- `@app.api_route("/files/{path:path}", PROXY_METHODS)` → covers `upload`, `archive`, `view`,
  `serve/{path}`, plus any `POST/DELETE` variants the named handlers don't list.
- `@app.api_route("/execute/{process_id}/{path:path}", PROXY_METHODS)` → covers `status`/`input`.
- `@app.api_route("/api/terminals/{session_id}", [GET,DELETE])`.

---

## 6. Minimal init-handshake set (to clear "Server connection failed")

Derived from the UI client (`AddTerminalServerModal.svelte`, `FileNav.svelte`,
`terminal/index.ts`):

1. **`GET /api/config`** — the **connection test**. `AddTerminalServerModal` calls
   `getTerminalConfig(url, key)`; any non-2xx / network failure → toast
   *"Server connection failed"*. **This is the single gate.** It needs `Authorization: Bearer`
   (proxy requires it; backend does not) but **no `X-User-Id`**.
2. **`GET /files/cwd`** (with `X-Session-Id: <chatId>` if a chat is open) — fetches the
   session cwd / home / root.
3. **`GET /files/list?directory=<cwd>`** (with `X-Session-Id`) — renders the file tree.

For an **interactive terminal pane** (separate from the file browser), the additional
handshake is `POST /api/terminals` (→ `{id}`) then the WS attach to
`/api/terminals/{id}` (WS protocol is out of scope here).

**So the broker's minimum to get the UI past its connection gate is to answer
`GET /api/config` with `200 {features:{terminal:true,...}}`.** A fully functional file
browser additionally needs `GET /files/cwd` and `GET /files/list`. Everything else is
lazy / user-driven.

---

## 7. Source-path quick reference

| Endpoint | Backend `main.py` | Proxy `main.py` |
|---|---|---|
| `GET /api/config` | `412` (no auth) | `347` (static, auth) |
| `GET /api/status` | — (absent) | `362` (static) |
| `GET /info` | `444` (conditional) | `386` (proxy) |
| `GET /ports` | `1342` | `690` (proxy) |
| `GET /files/cwd` | `458` | `397` (proxy) |
| `POST /files/cwd` | `478` | `405` (proxy) |
| `GET /files/list` | `502` | `421` (proxy) |
| `GET /files/read` | `528` | `436` (proxy) |
| `POST /files/write` | `666` | `470` (proxy) |
| `POST /files/replace` | `749` | `483` (proxy) |
| `POST /files/mkdir` | `677` | catch-all `542` |
| `POST /files/move` | `711` | catch-all `542` |
| `DELETE /files/delete` | `691` | catch-all `542` |
| `GET /files/grep` | `809` | `496` (proxy) |
| `GET /files/glob` | `922` | `523` (proxy) |
| `POST /files/upload` | `1015` | catch-all `542` |
| `POST /files/archive` | `1056` | catch-all `542` |
| `POST /execute` | `1147` | `580` (proxy) |
| `GET /execute/{id}/status` | `1216` | `604` (proxy) |
| `POST /execute/{id}/input` | `1274` | `625` (proxy) |
| `DELETE /execute/{id}` | `1305` | `646` (proxy) |
| `POST /api/terminals` | `1519` | `720` (proxy) |
| `GET /api/terminals` | `1635` | `720` (proxy) |
| `GET /api/terminals/{id}` | `1654` | `743` (proxy) |
| `DELETE /api/terminals/{id}` | `1670` | `743` (proxy) |
| `GET /api/v1/policies` | **absent** | **absent** |
