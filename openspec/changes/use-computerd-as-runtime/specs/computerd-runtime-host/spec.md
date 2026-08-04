## ADDED Requirements

### Requirement: Gateway shall own the authoritative virtual filesystem

The system SHALL keep exactly one authoritative dofs `Database` per worker, backed by a SQLite
file on a persistent volume, owned (written) by the gateway process alone. The `computerd`
sidecar SHALL hold only a synced FUSE mirror of that database; it SHALL NOT write the on-disk
SQLite directly. This single-writer invariant preserves dofs's correctness guarantees off the
Cloudflare Durable-Object runtime it was designed for.

#### Scenario: Single writer to the VFS database

- **WHEN** the worker starts
- **THEN** exactly one process (the gateway) SHALL open the dofs SQLite on the PVC for writes
  (WAL mode), and `computerd` SHALL access the filesystem only through its FUSE mirror synced via
  the capnweb rpc.

#### Scenario: VFS survives worker restart

- **WHEN** the gateway container restarts
- **THEN** it SHALL reopen the existing PVC SQLite, re-establish the `computerd` WebSocket, and
  resume serving sessions whose subtrees already exist, with no loss of previously written files.

### Requirement: One shared computerd serving all sessions

The system SHALL operate a single long-lived `computerd` sidecar per worker pod, shared across
all users and chats, rather than spawning a pod or container per session. Sessions SHALL be
represented as subtrees of the shared VFS (`/sess/<user_hash>/<chat>`), multiplexed onto the one
`computerd` instance.

#### Scenario: No per-session container is created

- **WHEN** a new (user, chat) session issues its first request
- **THEN** the system SHALL create only a VFS subtree and SHALL NOT create any new pod, container,
  Kubernetes Service, Secret, or PVC for that session.

### Requirement: Exec shall be bracketed by VFS sync

Every command execution SHALL push pending host-side VFS changes to the `computerd` mirror before
the command runs, and SHALL pull container-side writes back to the authoritative database after
the command completes, so that commands observe the latest files and their file writes are
captured durably.

#### Scenario: A command sees a file written via the REST API

- **WHEN** a client writes a file through `/files/write` and then runs a command that reads it
- **THEN** the command SHALL observe the file's current contents, because the pre-exec push
  delivered the change to the `computerd` mirror.

#### Scenario: A command's file writes are persisted

- **WHEN** a command writes a file inside the container and the command exits
- **THEN** the post-exec pull SHALL copy that write into the authoritative database, and a
  subsequent `/files/read` SHALL return it.

### Requirement: Gateway shall expose the open-terminal Phase-1 REST contract

The gateway SHALL implement the open-terminal REST endpoints and OpenAPI operation_ids consumed
by Open WebUI for the LLM tool surface: `/execute` (`run_command`), `/execute/{id}/status`
(`get_process_status`), `/execute/{id}/input` (`send_process_input`), `DELETE /execute/{id}`
(`kill_process`), `GET /execute` (`list_processes`), and the `/files/*` operations
(`list_files`, `read_file`, `write_file`, `replace_file_content`, `grep_search`, `glob_search`),
plus `/health`, `/api/config`, `/system`, `/info`, and `/openapi.json`. Request and response
shapes SHALL match open-terminal's so Open WebUI's native integration works without modification.

#### Scenario: Open WebUI builds the tool surface from the OpenAPI spec

- **WHEN** Open WebUI fetches the gateway's `/openapi.json`
- **THEN** it SHALL expose the same in-schema `operation_id`s and equivalent schemas as a stock
  open-terminal instance, allowing an LLM to call `run_command`, `read_file`, and `write_file`.

#### Scenario: Command execution follows the open-terminal polling contract

- **WHEN** a client `POST /execute`s a command
- **THEN** the gateway SHALL return a command identifier and paged output (via
  `/execute/{id}/status` with `next_offset`) consistent with open-terminal's one-shot execute
  contract, rather than requiring a streaming connection.

### Requirement: No Cloudflare-proprietary runtime dependency

The system SHALL NOT depend on Cloudflare Containers-for-Workers, Durable Objects, the Workers
runtime, or any Cloudflare account. The reused `dofs` and `rpc`/capnweb packages SHALL be vendored
into the repository under their MIT license with attribution preserved, and `computerd` SHALL be
consumed as a published container image on plain container infrastructure.

#### Scenario: The system runs without any Cloudflare binding

- **WHEN** the worker pod is deployed on MicroK8s
- **THEN** no `ctx.container`, `ctx.storage`, `DurableObject`, `cloudflare:workers` import, or
  Cloudflare account credential SHALL be required for the runtime to function.
