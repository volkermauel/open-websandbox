# Proposal: open-terminal client-API compatibility (stage 1: file browser)

## Why

Issue #164: Open Web UI's "Open Terminal" experience is built against the
reference server **open-terminal** (v0.12.3). Our runtime already implements
most of its file-verb surface, but recent upstream releases added the
endpoints OWUI's file browser actually drives — and they are missing here, so
the browser degrades (iframe previews break, search/picker UI errors, feature
detection falls back). We currently cannot make any honest "compatible with
open-terminal vX" statement; docs say nothing about the upstream contract at
all.

## What Changes

Stage 1 closes the OWUI-visible gaps (research-verified against upstream
`main` @ `c0273cc`, post-v0.12.3; report attached to #164):

- **`GET /files/serve/{*path}`** (upstream 0.11.34) — inline raw bytes with
  guessed content type, auth-protected, TOCTOU-safe open. Enables FileNav
  iframe previews with relative asset resolution.
- **`GET /api/config`** (0.8.1) — feature discovery, unauthenticated like
  upstream: `{"features":{"terminal":true,"notebooks":false,"system":false}}`.
- **`GET /files/list` enrichment** (0.11.35) — per-entry `writable` flag plus
  top-level `writable` for the listed directory (honest `access(W_OK)` probe
  via libc, respects read-only mounts and chmod).
- **`GET /files/read` line ranges + binary-awareness** (0.2.7 semantics) —
  optional 1-indexed inclusive `start_line`/`end_line`; non-UTF-8 files:
  images keep returning raw bytes, every other binary type now returns
  **415** (previously an opaque 500). Upstream's Office→text extraction is
  explicitly deferred.
- **`GET /files/search`** (0.11.36) — ranked filename search (exact → prefix →
  substring), git-aware when the workspace is a repo (gitignore honored,
  `git ls-files -co --exclude-standard`), dotfiles hidden unless
  `show_hidden`; `{"results":[{path,name,type,size,modified}]}`.
- **`GET /files/matches`** (0.12.0) — unified search: name matches (score 0–3)
  first, then content matches (case-insensitive literal, ≤1 MiB files, NUL
  sniffed binaries skipped, ≤3 matches/file, UTF-16 column numbers), paginated
  via `offset`/`limit` with `next_offset`.
- **Docs**: new `docs/compatibility.md` with an endpoint-by-endpoint matrix
  (upstream version attribution vs our status) and the statement:
  *"open-websandbox is client-API compatible with open-terminal v0.12.3 for
  the surface listed below."* Linked from README + mkdocs nav; CHANGELOG.

Explicitly **deferred** to follow-up issues: `/proxy/{port}/{path}` with
0.12.2 session-ownership, `GET /system`, `GET /info`, `/files/display`,
Office→PDF extraction, notebooks, multi-user OS provisioning.

## Impact

- Affected: `rust/runtime` (`files/{io,search}.rs`, `app.rs`, `openapi.rs`,
  `error.rs` — new 415 variant, `Cargo.toml` + libc), contract tests,
  `docs/`, `CHANGELOG.md`.
- Not affected: broker (catch-all relay forwards new routes unchanged),
  chart/base manifests (no config knobs).
- Wire-compat: purely additive (`/files/list` gains fields, `/files/read`
  gains params — both backwards compatible); the only behavior change is
  non-UTF-8 non-image reads moving 500 → 415, which matches upstream.
