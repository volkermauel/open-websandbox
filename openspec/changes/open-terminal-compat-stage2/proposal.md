# Proposal: open-terminal compatibility stage 2 — /system, /info, /files/display, /proxy/{port}

## Why

Issue #169 (follow-up to stage 1, #164/#170): the remaining open-terminal
v0.12.3 surfaces OWUI's terminal experience calls are still missing. `GET
/system` is the endpoint Open Web UI feeds to the model as the LLM system
prompt — without it the model gets no sandbox grounding at all. `GET /info`
carries operator context. `GET /files/display` is the "show this file to the
user" signal the model invokes from the tool surface. `/proxy/{port}` is the
"open port" button: a browser reaches a service the model started inside the
sandbox — upstream **0.12.2 locked it down** to ports owned by the caller's
own session processes, and that ownership rule is the security boundary.

## What Changes

Contracts ported **verbatim from upstream source** (`open_terminal/main.py`,
`env.py`, `utils/port.py` @ v0.12.3, clone at `/tmp/ott`):

- **`GET /system`** (upstream 0.11.27, template vars 0.11.35) — returns
  `{"prompt": <text>}`. The prompt text is the upstream default system prompt
  copied **byte-for-byte** (`get_system_prompt`, `main.py:96-118`), including
  the `{{var}}` template-variable set (`os`, `kernel`, `arch`, `hostname`,
  `user`, `shell`, `python_version`, `home`; unknown vars left verbatim) used
  when an operator overrides the prompt via `OPEN_TERMINAL_SYSTEM_PROMPT`.
  Provenance pinned in code + docs: *"system prompt upstream-verbatim @
  v0.12.3 (open_terminal/main.py)"*. Grounding values come from the sandbox
  environment (`uname`, `$USER`, `$SHELL`/runtime shell, `$HOME`, probed
  python3). One documented sentence-level divergence: upstream's sentence
  *"Python {python_version} is available."* is included only when a python3
  probe succeeds — our default runtime image ships no Python (debian
  bookworm-slim + libreoffice-nogui; verified no python3 in the dep tree), and
  the prompt must not claim an interpreter the model cannot run. With python3
  present the prompt is byte-for-byte upstream.
- **`GET /info`** (upstream 0.11.6) — `{"info": <text>}` from the
  `OPEN_TERMINAL_INFO` env var (mirroring the upstream name). Upstream
  registers the route only when the value is non-empty: unset ⇒ 404
  `{"detail":"Not Found"}` — mirrored exactly.
- **`GET /files/display`** (upstream 0.2.9) — query `path`, resolved against
  the workspace base; returns `{"path": <resolved>, "exists": bool}`. It is a
  **signaling** endpoint (the client opens its viewer); it does not serve
  bytes. Missing files are *not* 404 — `exists: false` is the contract.
- **`GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS /proxy/{port}[/{*path}]`**
  (upstream 0.9.0 route; **0.12.2 ownership lockdown**) — reverse-proxy to
  `http://localhost:{port}/{path}?{query}`. Ownership per upstream
  single-user semantics: only ports whose listening socket belongs to a
  **descendant process of the runtime** (the processes the session's
  `/execute`/PTY children started); unowned/unlistened ports ⇒ 404 `"Port not
  found"`. Port outside 1..=65535 ⇒ upstream's `"Port must be between 1 and
  65535"` (422 upstream, our documented global 400 divergence). Hop-by-hop
  headers stripped both ways; the inbound `Authorization` is **not** forwarded
  (upstream strips it). Connect errors ⇒ 502 `"Connection refused:
  localhost:{port}"`, timeouts (300 s total / 5 s connect) ⇒ 504 `"Timeout
  connecting to localhost:{port}"`.
- **`GET /ports` upgrade** — upstream feeds the *same* visibility function
  (`_visible_ports`) to `/ports` and the proxy ownership check; we now do the
  same: `/ports` lists the real session-visible listening ports (`{port, pid,
  process}`, uid stripped like upstream) instead of the stage-1 empty stub, so
  OWUI's port panel and the proxy agree.
- **`/api/config` flag flip** — `features.system` goes `false → true` in the
  runtime route **and** the broker-fronted `Features` (stage 1 shipped
  `false`); `/system` now exists, so the flag must stop lying.

Explicitly **out of scope** (stays deferred): notebooks, Office→text
extraction in `/files/view`, multi-user OS provisioning.

## Impact

- Affected: `rust/runtime` (new `system.rs` + `ports.rs` modules, `files/io.rs`
  `display_file`, `app.rs` routes, `state.rs` proxy client, `config.rs` `info`
  + `system_prompt` knobs, `openapi.rs`, `Cargo.toml`: reqwest no-default-features
  + nix `utsname`), `rust/broker` (`Features.system = true` + regenerated
  OpenAPI snapshot), contract tests, `tests/e2e`, `docs/compatibility.md`,
  `CHANGELOG.md`.
- Not affected: chart/base manifests (no new required knobs; `OPEN_TERMINAL_INFO`
  / `OPEN_TERMINAL_SYSTEM_PROMPT` are optional pass-throughs), the broker relay
  (catch-all `any()` already forwards `/system`, `/info`, `/files/display`,
  `/proxy/*` with every method).
- Wire-compat: additive routes plus one intentional flag flip (`system: true`)
  that matches the new reality; `/ports` changes from a stub `[]` to real
  listings (an upstream-parity improvement, flagged in the CHANGELOG).
