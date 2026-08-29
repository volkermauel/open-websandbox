# Design: open-terminal client-API compatibility (stage 1)

Upstream contracts verified in the reference server (`open_terminal/main.py`,
`utils/fs.py`, `env.py` @ v0.12.3). Divergences are called out explicitly.

## `GET /files/serve/{*path}` (0.11.34)

Auth-protected. Upstream = `view_file(path, preview=False)` → raw bytes with
`mimetypes.guess_type` (fallback `application/octet-stream`), **no
Content-Disposition** (inline; it exists for iframe asset resolution). Ours:
`safe_path` → is_file → `open_read` (TOCTOU-safe, #99 A5) →
`mime_guess` content type. Divergence from our `/files/view`: view keeps its
`attachment` disposition (download-oriented); serve mirrors upstream inline.

## `GET /api/config` (0.8.1)

Upstream is **unauthenticated** (no `verify_api_key` dependency) and returns
`{"features": {"terminal": bool, "notebooks": bool, "system": bool}}`. Ours:
static `{"terminal": true, "notebooks": false, "system": false}` — we always
serve terminals and implement neither notebooks nor a system-prompt endpoint.
Inside the cluster the route is only reachable through the broker relay
(broker auth still required there), so the unauthenticated surface is the
sandbox pod itself, exactly like upstream.

## `/files/list` enrichment (0.11.35)

Entry gains `writable: bool`; response gains top-level `writable: bool` for
the listed directory. Upstream `writable` = `os.access(path, W_OK)`; ours =
`libc::access(., W_OK)` (honest probe: read-only mounts, chmod 444, root
bypass all behave correctly). `libc` becomes a direct runtime dependency
(already compiled transitively by tokio; no build-cost change).

## `/files/read` line ranges + binary awareness (0.2.7)

New optional query params `start_line`, `end_line` (1-indexed, inclusive,
`>= 1`, default = whole file). Text: `total_lines` (of the whole file) +
sliced `content` (lines joined keepends). Non-UTF-8: images (`image/*` by
mime prefix) keep returning raw bytes; **any other binary → 415**
`Unsupported binary file type: {mime} ({n} bytes)` — new `ApiError`
variant `UnsupportedMediaType`. Previously this was an opaque 500.
Divergence (deferred): upstream attempts Office/document text extraction
first (LibreOffice in its image); we return 415 directly — documented in
compatibility.md.

## `GET /files/search` (0.11.36 semantics)

Params: `query` (default `""`), `path` (default `.`), `limit` 1..=100
(default 20), `type` one of `file|directory|any` (default `any`),
`show_hidden` (default false). Candidate walk: `git -C <target> ls-files -co
--exclude-standard -z -- .` when git exists **and** returns 0 (gitignore
honored; adds parent dirs of tracked files as directory candidates); else
plain `walkdir`-style recursion. Hidden = any path segment starting `.`.
Ranking: 0 name == query (case-insensitive), 1 name starts-with, 2 contains,
else skip; empty query matches everything at rank 2. Sort:
`(rank, name.len(), rel_path.to_lowercase())`, truncate to `limit`.
Response: `{"results": [{"path": <absolute>, "name", "type", "size",
"modified"}]}`. Out-of-range `limit`/bad `type` → 400 (upstream 422 —
documented divergence, clients never violate).

## `GET /files/matches` (0.12.0)

Params: `query` **required**, blank-after-strip → 400; `path` default `.`;
`show_hidden` default false; `offset` >= 0 default 0; `limit` 1..=100
default 100 (= upstream `MATCH_PAGE_SIZE`). Same candidate walk as search.
Content matches (files only, skip symlinks): file <= 1 MiB
(`MAX_CONTENT_SEARCH_FILE_SIZE`), skip if first 8 KiB contains a NUL byte,
read lossy-UTF-8, case-insensitive literal find per line, max 3 per file
(`MAX_CONTENT_MATCHES_PER_FILE`); match = `{"line", "column", "text"}` with
column = `utf16_code_units(line[..index]) + 1` (exact upstream mirror).
Upstream prefers `rg` when installed; our runtime image has none, so we
implement only the portable path (same semantics, lower throughput —
acceptable at sandbox scale, documented). Score: 0 name ==, 1 name
starts-with, 2 name contains, 3 relative-path contains, else 4 (content
only). Include a candidate iff score < 4 **or** it has content matches.
`name_match` = score < 4. Sort `(score, rel_path.len(), rel.to_lowercase())`,
slice `[offset, offset+limit]`, `next_offset = Some(offset+limit)` iff more
remain. Response: `{"results": [{"path", "relative_path", "name", "type",
"name_match", "content_matches"}], "next_offset": null|n}`.

## Testing

- Contract tests (router-level, tempdir workspace, bearer auth): serve
  inline bytes + content type; config shape + unauthenticated; list writable
  flags (chmod 000 file → false); read line-range slicing, 415 on binary
  non-image, image raw bytes unchanged; search ranking/filters/hidden;
  matches content hits + UTF-16 column + pagination + blank 400.
- e2e (new `test_open_terminal_compat.py`, runs in the matrix lanes via
  `pytest tests/e2e`, no extra env): through the **broker relay** — upload a
  text + binary + html file, `serve` roundtrip byte equality, `config`
  features, `list` writable, `search`/`matches` find a known uploaded name
  and a known content line. Proves the catch-all relay forwards the new
  routes with auth intact.

## Wire compatibility notes

All changes are additive except the read-500→415 fix, which *increases*
fidelity. OWUI clients gate features on `/api/config` and degrade gracefully
when endpoints 404 — stage 1 removes the 404s the file browser hits.
