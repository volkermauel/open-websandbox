# Tasks: open-terminal client-API compatibility (stage 1)

## 1. Runtime endpoints

- [x] 1.1 `error.rs`: add `UnsupportedMediaType(String)` → 415 variant
- [x] 1.2 `Cargo.toml`: direct `libc` dep; `files/mod.rs`: `is_writable(path)` helper (`access(W_OK)`)
- [x] 1.3 `files/io.rs`: `serve_file` handler (inline bytes + guessed mime, TOCTOU-safe open) + route
- [x] 1.4 `app.rs`: `GET /api/config` (unauthenticated, static features JSON)
- [x] 1.5 `files/io.rs`: `Entry.writable` + `ListResponse.writable`
- [x] 1.6 `files/io.rs`: `read_file` `start_line`/`end_line` params + 415 binary handling
- [x] 1.7 `files/search.rs`: `search_files` (0.11.36) — git-aware walk, ranking, filters
- [x] 1.8 `files/search.rs`: `match_files` (0.12.0) — content matches, UTF-16 columns, pagination
- [x] 1.9 `openapi.rs`: register new paths/schemas

## 2. Tests

- [x] 2.1 Contract tests: serve/config/list-writable/read-ranges+415/search/matches
- [ ] 2.2 e2e `test_open_terminal_compat.py` through the broker relay (matrix lanes pick it up automatically)

## 3. Docs & release

- [x] 3.1 `docs/compatibility.md` matrix + v0.12.3 statement; mkdocs nav; README link
- [x] 3.2 CHANGELOG `[Unreleased]`
- [x] 3.3 Follow-up issues for deferred surface (proxy, /system, /info, display, Office extraction, notebooks)

## 4. Gates

- [x] 4.1 `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --workspace`
- [ ] 4.2 KIND live lane: build images, focused pytest of the new e2e
- [x] 4.3 OpenSpec validate + PR (closes #164 stage 1)
