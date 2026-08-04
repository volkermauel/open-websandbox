# Open Terminal — Runtime Contract Research

Repository: `open-terminal` (cloned locally, tag/version `0.11.34` per `pyproject.toml`).

---

## 1. What it is

Open Terminal is a **self-hosted AI command-execution REST API** written in **Python 3.11+** on **FastAPI**, served by **Uvicorn** (the standard `[standard]` extra). Entry point: `open_terminal/main.py:app` (`open_terminal/cli.py:132` → `uvicorn.run("open_terminal.main:app", ...)`). It is packaged as a pip-installable Python package (`pyproject.toml`, `[project.scripts] open-terminal = "open_terminal.cli:main"`) AND as a family of Docker images published to `ghcr.io/open-webui/open-terminal` with four variants (`README.md:27-37`):

| Variant | Size | Bundled tooling | Runtime pkgs | Multi-user | Egress FW |
|---|---|---|---|---|---|
| `latest` | ~4 GB | Node, gcc, ffmpeg, LaTeX, Docker CLI, DS libs | ✔ (sudo) | ✔ | ✔ |
| `slim` | ~430 MB | git, curl, jq (Debian/glibc) | ✘ | ✘ | ✔ |
| `alpine` | ~230 MB | git, curl, jq (musl) | ✘ | ✘ | ✔ |
| `openshift` | ~430 MB | git, curl, jq | ✘ | ✘ | ✘ |

It can also run on **bare metal** (`pip install open-terminal` / `uvx open-terminal run`), in which case commands execute directly on the host with the running user's privileges (`README.md:11-12, 61-75`).

What it exposes: a single FastAPI `app` providing health, config discovery, an LLM-oriented **system prompt**, a **files API** (list/read/write/replace/grep/glob/mkdir/move/delete/upload/archive/display/view), a **command execution API** (run/list/status/input/kill), **interactive PTY terminal sessions over WebSocket**, an optional **Jupyter notebook** execution API, a localhost **port-detection + reverse-proxy** helper, and an optional **MCP server** (`open_terminal/mcp_server.py`) that mirrors every FastAPI route as MCP tools via `FastMCP.from_fastapi`.

---

## 2. HTTP API surface

All routes are defined with `@app.get/post/delete(...)` / `@app.api_route(...)` / `@router.*` in `open_terminal/main.py` (and `open_terminal/utils/notebooks.py`). Auth is applied per-route via `dependencies=[Depends(verify_api_key)]`. Routes marked `include_in_schema=False` are hidden from `/docs` / `/openapi.json` (they are UI helpers, not LLM tools).

### 2.1 Public / discovery

| Method | Path | Auth | Purpose | file:line |
|---|---|---|---|---|
| GET | `/health` | none | `{"status":"ok"}` | `main.py:392-400` |
| GET | `/api/config` | none | Feature flags `{terminal, notebooks, system}` | `main.py:408-420` |
| GET | `/system` | Bearer | LLM system prompt (if `ENABLE_SYSTEM_PROMPT`) | `main.py:425-432` |
| GET | `/info` | Bearer | Operator-provided env info (`OPEN_TERMINAL_INFO`) — exposed in schema | `main.py:437-445` |

### 2.2 Files (`/files/*`)

All Bearer-authenticated. Most are `include_in_schema=False` (UI); those with `operation_id` and in-schema are the LLM "tools":

| Method | Path | op id | in-schema | Purpose | file:line |
|---|---|---|---|---|---|
| GET | `/files/cwd` | — | no | Get session cwd + home + browser-root hint | `main.py:453-470` |
| POST | `/files/cwd` | — | no | Set session cwd (body `MkdirRequest{path}`) | `main.py:473-488` |
| GET | `/files/list?directory=` | `list_files` | **yes** | Structured dir listing | `main.py:491-513` |
| GET | `/files/read?path=&start_line=&end_line=` | `read_file` | **yes** | Read text / images / Office-PDF text extraction | `main.py:516-588` |
| GET | `/files/display?path=` | `display_file` | **yes** | Signal UI to open file (returns path+exists) | `main.py:591-617` |
| GET | `/files/view?path=` | — | no | Raw bytes with Content-Type (UI preview) | `main.py:620-643` |
| GET | `/files/serve/{path:path}` | — | no | Alias of view (iframe URLs) | `main.py:646-653` |
| POST | `/files/write` | `write_file` | **yes** | Write text file (body `WriteRequest{path,content}`) | `main.py:656-674` |
| POST | `/files/mkdir` | — | no | mkdir -p | `main.py:677-688` |
| DELETE | `/files/delete?path=` | — | no | rm file/dir | `main.py:691-708` |
| POST | `/files/move` | — | no | mv (body `MoveRequest`) | `main.py:711-734` |
| POST | `/files/replace` | `replace_file_content` | **yes** | Find/replace in file (body `ReplaceRequest`) | `main.py:737-794` |
| GET | `/files/grep` | `grep_search` | **yes** | Content search, regex/glob filters | `main.py:797-908` |
| GET | `/files/glob` | `glob_search` | **yes** | Filename glob search | `main.py:911-1010` |
| POST | `/files/upload?directory=` | `upload_file` | no | Multipart upload | `main.py:1015-1046` |
| POST | `/files/archive` | — | no | ZIP files into a download | `main.py:1056-1115` |

### 2.3 Command execution (`/execute/*`) — the core

**There is NO streaming endpoint for command output.** Execution is **one-shot POST that returns immediately with a `command id`**, then output is fetched by **HTTP polling** of a status endpoint. Output is persisted to a JSONL log file and drained on read. Shape:

`POST /execute?wait=&tail=` — `operation_id="run_command"` (`main.py:1147-1213`):

```python
# main.py:1157-1175
async def execute(http_request: Request, request: ExecRequest,
                  wait: Optional[float] = Query(None, ...),
                  tail: Optional[int] = Query(None, ...)):
    fs = get_filesystem(http_request)
    session_id = http_request.headers.get("x-session-id")
    session_cwd = _get_session_cwd(session_id, fs) if session_id else None
    cwd = fs.resolve_path(request.cwd, cwd=session_cwd) if request.cwd else (session_cwd or fs.home)
    subprocess_env = {**os.environ, **request.env} if request.env else None
    runner = await create_runner(request.command, cwd, subprocess_env, run_as_user=fs.username)
    process_id = time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:6]
    ...
```

Request body (`main.py:204-217`):

```python
class ExecRequest(BaseModel):
    command: str = Field(..., description="Shell command to execute. Supports chaining (&&, ||, ;), pipes (|), and redirections.")
    cwd: Optional[str] = Field(None, description="Working directory for the command. Defaults to the server's current directory if not set.")
    env: Optional[dict[str, str]] = Field(None, description="Extra environment variables merged into the subprocess environment.")
```

Response (`main.py:1204-1213`): `{id, command, status, exit_code, output[], truncated, next_offset, log_path}`.
The `wait` query param (`0..300s`, default `EXECUTE_TIMEOUT`) lets a client block inline for short commands; otherwise it returns immediately.

| Method | Path | op id | Purpose | file:line |
|---|---|---|---|---|
| GET | `/execute` | `list_processes` | List tracked background processes | `main.py:1123-1144` |
| POST | `/execute` | `run_command` | Start a command, return id (+optional inline output) | `main.py:1147-1213` |
| GET | `/execute/{id}/status?wait=&offset=&tail=` | `get_process_status` | **Poll** for new output (drained) + exit code | `main.py:1216-1271` |
| POST | `/execute/{id}/input` | `send_process_input` | Write to stdin (body `InputRequest{input}`) | `main.py:1274-1302` |
| DELETE | `/execute/{id}?force=` | `kill_process` | SIGTERM / SIGKILL | `main.py:1305-1328` |

**Streaming model:** stdout/stderr/stdin are NOT streamed over the HTTP connection. Output is captured by a background task (`log_process`) into a per-process JSONL file under `$LOG_DIR/processes/<id>.jsonl` (`main.py:1183`) and retrieved by polling `GET /execute/{id}/status?offset=` with `next_offset` paging. Each line is `{type:"stdout"|"stderr"|"output", data, ts}` (`runner.py:96-106`, `160-173`). Input is sent synchronously via `POST /execute/{id}/input`.

### 2.4 Interactive terminal sessions (persistent PTY) — WebSocket

`if ENABLE_TERMINAL:` block (`main.py:1450-1824`). This **is** the persistent-shell/keep-alive surface: a real PTY shell that survives across many keystrokes.

| Method | Path | Auth | Purpose | file:line |
|---|---|---|---|---|
| POST | `/api/terminals` | Bearer | Allocate a new PTY session → `{id, created_at, pid}` | `main.py:1519-1624` |
| GET | `/api/terminals` | Bearer | List live sessions | `main.py:1635-1651` |
| GET | `/api/terminals/{id}` | Bearer | Session info | `main.py:1654-1667` |
| DELETE | `/api/terminals/{id}` | Bearer | Kill session | `main.py:1670-1676` |
| **WS** | `/api/terminals/{id}` | **first-message auth** | Bidirectional PTY: client sends binary=keystrokes, text `{type:"resize",cols,rows}`, first text frame `{type:"auth",token}`; server sends binary PTY output | `main.py:1679-1824` |

All `include_in_schema=False` (terminal sessions are a UI feature, not an LLM tool). Session cap: `MAX_TERMINAL_SESSIONS` (default 16, `env.py:62-67`). Backend selection (`main.py:1462-1471`): Unix `pty` (preferred) → `pywinpty` (Windows) → `None` (503).

### 2.5 Port detection + reverse proxy (UI helper)

| Method | Path | Purpose | file:line |
|---|---|---|---|
| GET | `/ports` | Listening TCP ports on localhost (filtered by user/descendants) | `main.py:1337-1374` |
| ANY | `/proxy/{port}/{path:path}` | Reverse-proxy to `http://localhost:{port}/{path}` via httpx | `main.py:1392-1443` |

### 2.6 Notebooks (`/notebooks/*`) — optional, `ENABLE_NOTEBOOKS`

Router in `utils/notebooks.py`, prefix `/notebooks`, all Bearer auth, all `include_in_schema=False`. Spawns a real Jupyter kernel via `nbclient`/`ipykernel` in-process (`notebooks.py:159-163`).

| Method | Path | Purpose | file:line |
|---|---|---|---|
| POST | `/notebooks` | Create kernel session for a `.ipynb` | `notebooks.py:131-175` |
| POST | `/notebooks/{id}/execute` | Execute one cell | `notebooks.py:177-249` |
| GET | `/notebooks/{id}` | Status | `notebooks.py:251-269` |
| DELETE | `/notebooks/{id}` | Stop kernel | `notebooks.py:271-281` |

### 2.7 Session / workdir concept

There is a **per-session working directory** keyed by the `X-Session-Id` request header (not auth — a client-chosen correlation id). In-memory dict `_session_cwds: {session_id: (abs_path, ts)}` with TTL `SESSION_CWD_TTL` (default 7 days, `env.py:171-176`):

```python
# main.py:320-345
_session_cwds: dict[str, tuple[str, float]] = {}
def _get_session_cwd(session_id, fs) -> str:   # defaults to fs.home
def _set_session_cwd(session_id, path):        # set via POST /files/cwd
```

The execute path honours it: `cwd = request.cwd or session_cwd or fs.home` (`main.py:1175`). Note: a later command only "sees" earlier files because they share the same **process filesystem** (the container/host disk), not because of any per-session chroot — the session id only tracks the *current directory pointer*.

---

## 3. Authentication

- **Single shared bearer token.** Validated in `verify_api_key` (`main.py:124-130`):

  ```python
  bearer_scheme = HTTPBearer(auto_error=False)   # main.py:121
  async def verify_api_key(credentials = Depends(bearer_scheme)):
      if not API_KEY: return
      if not credentials or not hmac.compare_digest(credentials.credentials, API_KEY):
          raise HTTPException(status_code=401, detail="Invalid API key")
  ```

  - **Header:** standard HTTP `Authorization: Bearer <key>` (FastAPI `HTTPBearer`).
  - Constant-time compare (`hmac.compare_digest`).
  - Source of `API_KEY`: `_resolve_file_env("OPEN_TERMINAL_API_KEY", config.get("api_key",""))` (`env.py:31`), with Docker-secrets `OPEN_TERMINAL_API_KEY_FILE` support. If unset at startup → `SystemExit` (`main.py:35-41`); the CLI auto-generates one with `secrets.token_urlsafe(48)` if still empty (`cli.py:87-89`).
  - WebSocket auth is **first-message**: client must send `{"type":"auth","token":"<key>"}` as first text frame within 10 s (`main.py:1709-1718`).
- **Multi-user mode** = `OPEN_TERMINAL_MULTI_USER=true` (`env.py:144-147`). When on, a startup check runs (`main.py:31-33` → `check_environment()`, `user_isolation.py:35-55`): requires Linux, `useradd`, and either root or `sudo`.
- **How multi-user isolates users** — **separate OS users, one container** (NOT separate processes, NOT separate containers):
  - Each request's `X-User-Id` header is hashed/sanitised to a Linux username (`user_isolation.py:58-79` `sanitize_username`).
  - `ensure_os_user` runs `useradd -m -s /bin/bash <name>`, `chown -R`, `chmod 2770` on `/home/<name>`, and `usermod -aG <name> <server_user>` so the server process can read natively (`user_isolation.py:82-138`).
  - `get_filesystem()` (`main.py:133-146`) returns a `UserFS(username=..., home=/home/<name>)` when `X-User-Id` is present.
  - Commands run as that user via **`sudo -u <name> -- bash -c "<cmd>"`** — see `runner.py:60-65`:

    ```python
    if run_as_user:
        inner = f"cd {shlex.quote(cwd)} && {command}" if cwd else command
        command = f"sudo -u {shlex.quote(run_as_user)} -- bash -c {shlex.quote(inner)}"
        cwd = None
    ```

  - File writes use native Python I/O then `sudo chown`/`sudo chmod g+w` to fix ownership (`fs.py:93-110`).
  - Kernel-enforced isolation is just Unix permissions on `chmod 2770` home dirs; the README explicitly warns this is **not** a real security boundary (`README.md:189-190`).
- The `X-User-Id` and `X-Session-Id` headers are **trusted plaintext** from the client (Open WebUI backend sets them when proxying). There is no per-user key — one `OPEN_TERMINAL_API_KEY` gates the whole instance.

---

## 4. Execution model

**open-terminal itself IS the sandbox host.** It executes commands **locally, in its own process tree, on the same kernel/filesystem where the FastAPI server runs.** There is **no pluggable backend/executor/runtime abstraction** — execution is hard-wired to local subprocess/PTY creation.

- The only seam is `create_runner()` in `open_terminal/utils/runner.py:283-296`:

  ```python
  async def create_runner(command, cwd, env, run_as_user=None) -> ProcessRunner:
      if _PTY_AVAILABLE:
          return PtyRunner(command, cwd, env, run_as_user=run_as_user)   # Unix
      if _WINPTY_AVAILABLE:
          return WinPtyRunner(command, cwd, env)                          # Windows
      runner = PipeRunner(command, cwd, env); await runner.start()        # fallback
      return runner
  ```

  `ProcessRunner` is an ABC with `read_output / write_input / kill / wait / close / pid` (`runner.py:28-54`) — three concrete impls: `PtyRunner`, `PipeRunner`, `WinPtyRunner`. **All three spawn a real local child** (`subprocess.Popen` / `asyncio.create_subprocess_shell` / `WinPtyProcess.spawn`).
- `PtyRunner.__init__` (`runner.py:60-85`): `pty.openpty()` → `subprocess.Popen(command, shell=True, stdin=stdout=stderr=slave_fd, cwd=cwd, env=env, start_new_session=True)`. Multi-user rewrites the command to `sudo -u <user> -- bash -c "..."`.
- Background-process bookkeeping is in-memory module state (`main.py:310-384`): `_processes: dict[id, BackgroundProcess]`, auto-cleaned after `_EXPIRY_SECONDS=300`. A `log_process` task streams the child's output to a JSONL file; clients poll it.
- Interactive PTY terminals (`main.py:1519-1624`) spawn `subprocess.Popen([SHELL] or ["script","-qc","sudo -i -u <user>","/dev/null"], stdin/out/err=slave_fd, start_new_session=True)` — again, a local child of the server.
- Notebooks (`notebooks.py:159-163`) start an in-process **Jupyter kernel** (`ipykernel`) via `nbclient.NotebookClient`.
- Port proxy (`main.py:1392-1443`) proxies to `http://localhost:{port}` — i.e. it assumes spawned servers live on the **same loopback** as the open-terminal process.

There is **no place** where "where does this command run?" is a configurable decision — it is always "right here, as a child of this Python process."

---

## 5. Filesystem / workdir

- **No per-request sandbox FS.** All file ops target the real local filesystem through `UserFS` (`utils/fs.py`), which is just stdlib `aiofiles`/`os`/`shutil` plus optional `sudo chown` for ownership fixups.
- **Working directory:** resolved per request as `request.cwd → session_cwd → fs.home` (`main.py:1175`). `fs.home` defaults to `os.getcwd()` for the server process (`fs.py:35`); in Docker it is `/home/user` (`Dockerfile:71 USER user`; volume `-v open-terminal:/home/user` in `README.md:19`).
- **Per-session persistence:** yes — but only because sessions share the container/host disk. `_session_cwds` (`main.py:320`) tracks a *cwd pointer* keyed by `X-Session-Id` (TTL 7 days). A later command sees earlier files iff they were written to the same real path. There is no overlay/copy-on-write layer.
- **Volumes:** the Docker run examples bind-mount `/home/user` (`README.md:19, 106`) or `/home` for multi-user (`README.md:196`). Persistent state = whatever is on that volume.
- **Runtime package install** (image extension), from `entrypoint.sh:57-82`:
  - `OPEN_TERMINAL_PACKAGES` → `sudo apt-get install -y`
  - `OPEN_TERMINAL_PIP_PACKAGES` → `pip install` (sudo when multi-user)
  - `OPEN_TERMINAL_NPM_PACKAGES` → `npm install -g` (sudo when multi-user)
  - Only the `latest` image has `sudo`, so only `latest` supports these (`README.md:34, 47`).
- **Path safety in multi-user:** `UserFS.is_path_allowed` (`fs.py:71-84`) blocks paths under `/home/<other_user>/`. The `/home/user` and `/home/usr` literals are auto-rewritten to the real user home (`fs.py:53-65`) to tolerate LLM hallucinations.

---

## 6. How Open WebUI uses it

From `README.md:167-205`:

- Open WebUI integrates Open Terminal as a **first-class "Open Terminal" connection type** under **Settings → Integrations → Open Terminal** — **not** as a generic "Tool server" / MCP server. Adding it as Open Terminal gives the UI a built-in file-navigation sidebar.
- **Two connection modes:**
  1. **Direct (per-user):** User Settings → Integrations → Open Terminal; user pastes **URL + API key**; requests go **browser → open-terminal directly** (`README.md:171-177`).
  2. **System-level (admin, multi-user):** Admin Settings → Integrations → Open Terminal; admin pastes **URL + API key**; requests are **proxied through the Open WebUI backend**, which injects the `X-User-Id` header so open-terminal can map to an OS user (`README.md:179-186`). Multiple terminals can be configured with per-user/group access control.
- **What Open WebUI expects:** a **REST API with an OpenAPI spec** (FastAPI serves `/openapi.json` and `/docs`). The LLM-facing tool surface is exactly the in-schema `operation_id`s: `run_command`, `list_processes`, `get_process_status`, `send_process_input`, `kill_process`, `list_files`, `read_file`, `write_file`, `replace_file_content`, `grep_search`, `glob_search`, `display_file`, `get_info` (see §2). The hidden (`include_in_schema=False`) routes — terminals, notebooks, ports, proxy, cwd, upload, etc. — are consumed by the Open WebUI **frontend**, not by the LLM.
- **Auth flow Open WebUI uses:** `Authorization: Bearer <OPEN_TERMINAL_API_KEY>` on every call; for system-level multi-user it additionally forwards `X-User-Id` (and `X-Session-Id`) so the backend can route to the right OS user/cwd.
- An **MCP server** also exists (`open_terminal/mcp_server.py`, `cli.py:135-187` `open-terminal mcp`) wrapping all FastAPI routes via `FastMCP.from_fastapi` — usable by any MCP client, but Open WebUI's native integration uses the REST/OpenAPI path, not MCP.

---

## 7. Configuration / env vars

Resolution order (highest wins): CLI flag → env var (or `<VAR>_FILE` Docker secret) → user TOML (`$XDG_CONFIG_HOME/open-terminal/config.toml`) → system TOML (`/etc/open-terminal/config.toml`) → defaults (`config.py:1-11`, `README.md:120-128`).

| Var | Default | Meaning | source |
|---|---|---|---|
| `OPEN_TERMINAL_API_KEY` (+ `_FILE`) | (auto-gen) | Bearer token; gates all auth'd routes | `env.py:31`, `cli.py:87-89` |
| `OPEN_TERMINAL_CORS_ALLOWED_ORIGINS` | `*` | Comma-separated allowed origins | `env.py:32-35` |
| `OPEN_TERMINAL_LOG_DIR` | `$XDG_STATE_HOME/open-terminal/logs` | Root for `processes/*.jsonl` logs | `env.py:36-49` |
| `OPEN_TERMINAL_BINARY_MIME_PREFIXES` | `image` | Mime prefixes returned raw by `read_file` | `env.py:53-60` |
| `OPEN_TERMINAL_MAX_SESSIONS` | `16` | Cap on live interactive PTY sessions | `env.py:62-67` |
| `OPEN_TERMINAL_ENABLE_TERMINAL` | `true` | Enable `/api/terminals*` WebSocket PTY | `env.py:69-72` |
| `OPEN_TERMINAL_TERM` | `xterm-256color` | `$TERM` for spawned shells | `env.py:74-77` |
| `OPEN_TERMINAL_EXECUTE_TIMEOUT` | unset | Default `wait` seconds for `/execute` | `env.py:79-85` |
| `OPEN_TERMINAL_EXECUTE_DESCRIPTION` | `""` | Extra text appended to the `run_command` OpenAPI description | `env.py:87-90` |
| `OPEN_TERMINAL_MAX_LOG_SIZE` | `50000000` | Max bytes per process JSONL log | `env.py:94-99` |
| `OPEN_TERMINAL_LOG_RETENTION` | `604800` (7d) | How long to keep finished-process logs | `env.py:103-108` |
| `OPEN_TERMINAL_LOG_FLUSH_INTERVAL` / `_BUFFER` | `0` | Batching for log flushes | `env.py:113-127` |
| `OPEN_TERMINAL_ENABLE_NOTEBOOKS` | `true` | Enable `/notebooks*` Jupyter API | `env.py:129-132` |
| `OPEN_TERMINAL_ENABLE_SYSTEM_PROMPT` | `true` | Enable `GET /system` | `env.py:134-137` |
| `OPEN_TERMINAL_SYSTEM_PROMPT` | `""` | Custom LLM system prompt template (`{{os}}`, `{{user}}`, …) | `env.py:139-142` |
| `OPEN_TERMINAL_MULTI_USER` | `false` | Per-OS-user isolation via `sudo -u` (Linux+sudo/root only) | `env.py:144-147` |
| `OPEN_TERMINAL_USER_PREFIX` | `""` | Prefix for generated Linux usernames | `env.py:149-152` |
| `OPEN_TERMINAL_UVICORN_LOOP` | `auto` | Uvicorn event loop (`auto`/`asyncio`/`uvloop`) | `env.py:154-157` |
| `OPEN_TERMINAL_INFO` | `""` | Operator env info surfaced at `GET /info` | `env.py:159-162` |
| `OPEN_TERMINAL_FILE_BROWSER_ROOT` | `home` | UI hint for browser root (`home`/path/`{{home}}/x`/`filesystem`) | `env.py:164-167` |
| `OPEN_TERMINAL_SESSION_CWD_TTL` | `604800` (7d) | TTL for `_session_cwds` entries | `env.py:171-176` |
| `OPEN_TERMINAL_PACKAGES` / `_PIP_PACKAGES` / `_NPM_PACKAGES` | — | apt/pip/npm install at container start (`entrypoint.sh:57-82`; `latest` only) | `README.md:89-93` |
| `OPEN_TERMINAL_ALLOWED_DOMAINS` | unset | Egress firewall: unset=full, `""`=block all, `a,b`=DNS whitelist+iptables (`entrypoint.sh:84-163`) | `entrypoint.sh` |
| CLI `--host` / `--port` / `--api-key` / `--cors-allowed-origins` / `--cwd` / `--config` | `0.0.0.0` / `8000` / … | `open-terminal run` flags | `cli.py:25-60` |

---

## 8. Swappability assessment

**Goal:** run open-terminal's FastAPI server *outside* the sandbox and proxy command execution *into* per-user/per-chat Cloudflare `computer` instances.

**Verdict: NOT cleanly separable as-is — it is tightly coupled to being the sandbox host.** The execution site is never a configurable decision; it is implicit in dozens of direct local-kernel/local-filesystem calls. To rehost the backend you must introduce the abstraction layer that does not exist today.

### The code that decides where commands execute

There is no single dispatch line — the decision is *structural*: every execution path constructs a local child of the Python process.

1. **`create_runner()`** (`open_terminal/utils/runner.py:283-296`) — the *only* factory, hard-wired to local PTY/subprocess:

   ```python
   async def create_runner(command, cwd, env, run_as_user=None) -> ProcessRunner:
       if _PTY_AVAILABLE:
           return PtyRunner(command, cwd, env, run_as_user=run_as_user)
       if _WINPTY_AVAILABLE:
           return WinPtyRunner(command, cwd, env)
       runner = PipeRunner(command, cwd, env); await runner.start()
       return runner
   ```

   `PtyRunner` (`runner.py:60-85`) calls `pty.openpty()` + `subprocess.Popen(..., shell=True, cwd=cwd, env=env)`. The `run_as_user` param bakes in the assumption that the target user is a **local OS account** (`sudo -u <user>`).

2. **`execute()`** (`main.py:1178-1180`) — the only caller of `create_runner`, and it derives `run_as_user` from a *local* `UserFS`:

   ```python
   runner = await create_runner(request.command, cwd, subprocess_env, run_as_user=fs.username)
   ```

3. **Interactive PTY terminals** (`main.py:1575-1583`) spawn their own `subprocess.Popen([SHELL], stdin=slave_fd, …, start_new_session=True)` directly — they don't even use `create_runner`.

4. **`UserFS`** (`utils/fs.py`) — all file ops are `aiofiles`/`os`/`shutil` against the **local disk**, with `sudo chown`/`sudo chmod` for ownership. No remote-FS implementation exists.

5. **Port proxy** (`main.py:1392-1403`) hard-codes `http://localhost:{port}` — assumes the spawned service is on the server's loopback.

6. **Notebooks** (`notebooks.py:159-163`) start an in-process `ipykernel`.

### What "swappable" would require

The one clean seam that *does* exist is the `ProcessRunner` ABC (`runner.py:28-54`: `read_output / write_input / kill / wait / close / pid`). A `ComputerRunner(ProcessRunner)` could in principle translate each method into Cloudflare `computer` API calls — **but only for `/execute`**. To fully rehost you must also:

- Replace `UserFS` with an abstraction that talks to the remote computer's FS (read/write/list/grep/glob/replace/mkdir/move/delete/upload/archive) — currently ~15 endpoints of raw local I/O.
- Replace the WebSocket PTY (`/api/terminals/{id}`) — currently a real `pty.openpty()` fd pair — with a stream bridge to a remote PTY.
- Replace `detect_listening_ports` / `port_proxy` (loopback assumptions) — a remote computer's ports are not on the server's localhost.
- Rework multi-user isolation (`user_isolation.py`) — `useradd`/`sudo -u`/`chmod 2770` are meaningless against a remote computer; isolation would instead be "one computer per user," which is exactly your target topology but requires dropping the entire OS-user layer.
- Rethink `_processes` / `_session_cwds` in-memory state — they are process-local dicts (`main.py:310, 320`), so a horizontally-scaled proxy layer would need shared state keyed by `X-Session-Id`.

### Bottom line for the migration

The **HTTP/OpenAPI contract** Open WebUI consumes (the in-schema `operation_id`s in §2.3 + files tools, Bearer auth, `X-User-Id`/`X-Session-Id` headers) is clean and stable and **can be preserved verbatim**. The implementation behind it cannot. You have two realistic options:

- **(A) Keep the contract, rewrite the backend.** Fork open-terminal and inject a `Backend`/`Computer` abstraction under `create_runner` + `UserFS` + the PTY WS handler + port proxy, routing per `X-User-Id`/`X-Session-Id` to a dedicated Cloudflare `computer`. Significant surgery across `main.py`, `runner.py`, `fs.py`, `user_isolation.py`, `notebooks.py`.
- **(B) Thin proxy, not open-terminal.** Write a fresh FastAPI that re-exposes the same OpenAPI operation_ids and delegates each call to the Cloudflare `computer` API, reusing open-terminal's Pydantic models and route shapes as a spec but not its code. Lower coupling risk; you own all the glue.

Either way, open-terminal's value to you is its **API contract** (§2) and **Open WebUI integration protocol** (§6), not its execution code — that code assumes it *is* the box the commands run in.

---

## TL;DR

- **What:** FastAPI/Uvicorn Python service (`open_terminal/main.py:app`) shipped as pip package + 4 Docker images (`latest`/`slim`/`alpine`/`openshift`); exposes a files API, a command-execution API, interactive PTY WebSocket sessions, optional Jupyter notebooks, and a localhost port-proxy.
- **Execution API:** one-shot `POST /execute` returning a command id; output retrieved by **polling** `GET /execute/{id}/status` (JSONL log, `next_offset` paging); stdin via `POST /execute/{id}/input`; kill via `DELETE`. **No SSE/chunked streaming for commands** — only the WebSocket PTY streams.
- **Interactive shell:** yes — `POST /api/terminals` + `WS /api/terminals/{id}` (real PTY, first-message auth, binary keystrokes, resize text frames).
- **Auth:** single `Authorization: Bearer $OPEN_TERMINAL_API_KEY` (`hmac.compare_digest`); multi-user mode maps `X-User-Id` → local Linux user via `useradd`+`sudo -u` (kernel perms only; README warns it's not a real security boundary).
- **Execution site:** hard-wired local — `create_runner()` (`runner.py:283`) always returns a `PtyRunner`/`PipeRunner`/`WinPtyRunner` that `subprocess.Popen`s a child of the server. No backend/runtime abstraction.
- **FS:** real local disk via `UserFS` (stdlib + `sudo chown`); per-session cwd tracked in-memory by `X-Session-Id` header; persistence = shared container/host disk + Docker volume on `/home`.
- **Open WebUI:** integrates via **OpenAPI/REST** (not MCP, not a "tool server") as a first-class "Open Terminal" connection (direct browser, or backend-proxied with `X-User-Id`). LLM tool surface = the in-schema `operation_id`s.
- **Swap verdict:** the **API contract is portable**; the **implementation is not** — it is structurally coupled to being the sandbox host (local subprocess, local FS, loopback port proxy, OS-user isolation). Needs a new `Backend` abstraction (option A) or a clean re-implementation that only reuses the OpenAPI shape (option B).

## Swappability verdict

**Not separable as-is.** Open Terminal's HTTP/OpenAPI contract (Bearer auth, `X-User-Id`/`X-Session-Id` headers, the `/execute` + `/files/*` + `/api/terminals` route shapes) is clean and could be preserved verbatim against a Cloudflare `computer` backend, but the implementation has no execution-backend abstraction: `create_runner()` (`runner.py:283-296`) unconditionally returns a local `PtyRunner`/`PipeRunner` that `subprocess.Popen`s a child of the server, `UserFS` (`fs.py`) does raw local-disk I/O with `sudo chown`, the WebSocket PTY (`main.py:1575-1583`) opens a real `pty.openpty()` fd pair, and the port proxy (`main.py:1403`) assumes `localhost:{port}`. Multi-user isolation is even more host-bound — it provisions local Linux users via `useradd`/`sudo -u`. Rehosting means either forking to inject a `Backend`/`Computer` seam under `create_runner` + `UserFS` + the PTY WS + port proxy (substantial surgery across 5 modules) or building a thin fresh FastAPI that only reuses open-terminal's Pydantic models and OpenAPI shape as a spec. The reusable prize is the API contract, not the code.
