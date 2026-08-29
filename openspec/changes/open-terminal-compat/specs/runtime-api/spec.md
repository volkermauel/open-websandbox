# runtime-api

## ADDED Requirements

### Requirement: open-terminal file-browser surface

The runtime SHALL serve the open-terminal v0.12.3 client API subset
documented in `docs/compatibility.md`, so Open Web UI's file browser
(FileNav) and terminal experience work against open-websandbox unmodified.

#### Scenario: serving a file inline for an iframe

- **WHEN** `GET /files/serve/{path}` targets an existing workspace file with
  a valid broker session
- **THEN** the response is the raw file bytes with the guessed content type
  and no `Content-Disposition: attachment` header
- **AND** a missing file, a path escaping the workspace, or a missing
  session returns 404/400/401 respectively

#### Scenario: ranked filename search

- **WHEN** `GET /files/search?query=<q>` runs over a directory
- **THEN** results are ranked exact-name → name-prefix → name-contains
  (case-insensitive), tie-broken by `(name length, relative path)`,
  honor `type`/`show_hidden`/`limit` (1..=100, default 20), and skip
  gitignored entries when the workspace is a git repository

#### Scenario: unified name+content search

- **WHEN** `GET /files/matches?query=<q>` runs with a non-blank query
- **THEN** files matching by name (score 0–3) or content (case-insensitive
  literal, ≤1 MiB non-binary files, ≤3 line matches each, UTF-16 code-unit
  columns) are returned sorted by `(score, relative path)`, paginated via
  `offset`/`limit` with `next_offset`, and a blank query returns 400

### Requirement: honest file metadata

Listings and reads SHALL report file state the way open-terminal does.

#### Scenario: writability flags

- **WHEN** `GET /files/list` lists a directory
- **THEN** the response carries a top-level `writable` for the directory and
  a per-entry `writable`, both from a real `access(W_OK)` probe (chmod and
  read-only mounts are reflected)

#### Scenario: line ranges and binary rejection

- **WHEN** `GET /files/read` receives `start_line`/`end_line` (1-indexed,
  inclusive, ≥1)
- **THEN** `content` holds exactly those lines (keepends) while
  `total_lines` counts the whole file, a zero value returns 400, and a
  non-image binary file returns 415 `Unsupported binary file type` instead
  of an opaque 500

### Requirement: feature discovery

The platform SHALL expose open-terminal 0.8.1 feature discovery.

#### Scenario: config shape

- **WHEN** `GET /api/config` is called (runtime directly, or through the
  broker relay)
- **THEN** the response is `{"features":{"terminal":true,"notebooks":false,"system":false}}`,
  matching the v0.12.3 key set, and the runtime route requires no
  authentication exactly like upstream
