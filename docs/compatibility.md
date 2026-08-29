# open-terminal compatibility

Open Web UI's *Open Terminal* experience is built against the reference
sandbox server [open-terminal](https://github.com/open-webui/open-terminal).
**open-websandbox is client-API compatible with open-terminal v0.12.3 for the
surface listed below** — verified against upstream `main` @ `c0273cc`
(post-v0.12.3); the gap research is attached to issue #164.

## Architecture note

Upstream runs one open-terminal server per sandbox with a per-sandbox Bearer
API key. open-websandbox fronts the same client surface through the
**broker**: Open Web UI authenticates once (broker Bearer + `X-User-Id` /
`X-Session-Id` headers), the broker resolves the chat's sandbox and relays
every route listed below to the runtime inside it. File and terminal
endpoints therefore behave per-chat by construction — upstream's
`X-Session-Id` working-directory semantics map to the per-session sandbox
itself.

## Endpoint matrix

| Endpoint | Upstream since | Status | Notes |
|---|---|---|---|
| `POST /execute` | 0.1.0 | ✅ | Synchronous result (no `GET /execute/{id}` polling) |
| `GET /health` | 0.1.0 | ✅ | Ours is `GET /healthz` |
| `GET /files/cwd` · `POST /files/cwd` | 0.2.7 | ✅ | |
| `GET /files/list` | 0.2.0 | ✅ | + per-entry & top-level `writable` (0.11.35) |
| `GET /files/read` | 0.2.0 | ✅ | `start_line`/`end_line` (0.2.7); images raw; other binaries **415** |
| `POST /files/write` | 0.2.0 | ✅ | |
| `POST /files/mkdir` | 0.2.7 | ✅ | |
| `POST /files/move` | 0.4.2 | ✅ | 409 on existing destination |
| `DELETE /files/delete` | 0.2.7 | ✅ | |
| `GET /files/view` | 0.2.7 | ✅ | Download-oriented (`attachment`) |
| `GET /files/serve/{path}` | 0.11.34 | ✅ | Inline bytes + content type (FileNav iframes) |
| `POST /files/replace` | — | ✅ | Extension of ours |
| `GET /files/grep` | 0.2.6 | ✅ | |
| `GET /files/glob` | 0.2.6 | ✅ | |
| `GET /files/search` | 0.11.36 | ✅ | Ranked filename picker; gitignore-aware in repos |
| `GET /files/matches` | 0.12.0 | ✅ | Name + content matches, `next_offset` pagination |
| `POST /files/archive` | 0.11.28 | ✅ | ZIP bundle |
| `POST /files/upload` | 0.2.0 | ✅ | Multipart, `directory` param |
| `GET /files/display` | 0.2.9 | ❌ deferred | Show-file signaling |
| `GET /ports` | 0.9.0 | ✅ | Restricted runtime: returns an empty list |
| `GET /proxy/{port}/{path}` | 0.9.0 | ❌ deferred | Needs 0.12.2 session-ownership semantics |
| `POST/GET/DELETE /api/terminals` + WS | 0.7.0 | ✅ | PTY sessions; first-message WS auth, binary framing |
| `GET /api/config` | 0.8.1 | ✅ | Broker-fronted; v0.12.3 key set (`terminal`/`notebooks`/`system`) |
| `GET /system` | 0.11.27 | ❌ deferred | LLM grounding |
| `GET /info` | 0.11.6 | ❌ deferred | Operator context |
| `/notebooks` | 0.10.0 | ❌ deferred | Reported `false` in `/api/config` |
| `GET /snapshot` · `PUT /restore` | — | ➕ | Extension of ours (S3 tiering) |

## Documented divergences

- **Validation status code**: out-of-range/invalid query parameters return
  `400` (axum) where upstream's FastAPI returns `422`. Real clients never
  violate these bounds.
- **No `rg` fast path** in `/files/matches`: content search always uses the
  portable scanner (≤1 MiB files, NUL sniffed, ≤3 matches/file). Same
  results, lower throughput at repo scale.
- **No document extraction** in `/files/read`: upstream converts
  Office/PDF to text via baked-in LibreOffice before 415-ing; we return 415
  directly for non-image binaries.
- **Multi-user OS provisioning** (upstream 0.11.x `sudo -u` per user) is not
  implemented: isolation is per-sandbox (one runtime user per chat).
