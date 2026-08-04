## ADDED Requirements

### Requirement: Every exec shall be confined by an nsjail jail

Each command execution SHALL be wrapped in an `nsjail` invocation (applied by the gateway via
command construction, without modifying `computerd`) that confines the process to the session's
VFS subtree, runs with a per-session identity, and operates inside a restricted capability and
syscall surface.

#### Scenario: A command can only see its own session's files

- **WHEN** two sessions of the same user run concurrently
- **THEN** each command's filesystem view SHALL expose only its own session subtree (bound at
  `/workspace`), and SHALL NOT be able to read or write another session's subtree at the
  application level.

#### Scenario: The root filesystem is not writable outside the workspace

- **WHEN** a command attempts to write outside `/workspace` (for example to `/etc` or `/`)
- **THEN** the write SHALL fail, because the jail mounts the root filesystem read-only and
  exposes only `/workspace` and a per-exec `/tmp` tmpfs as writable.

### Requirement: Each session shall run under a distinct non-root identity

The jail SHALL assign each (user, chat) session a deterministic, distinct uid and gid inside a
non-root range, and the session's VFS subtree SHALL be owned by that identity, so processes of
different sessions cannot signal, ptrace, or otherwise interfere with each other by default.

#### Scenario: Processes of different sessions have different identities

- **WHEN** a command in session A inspects its identity (`id`)
- **THEN** it SHALL report the uid/gid allocated to session A, which SHALL differ from that of any
  concurrently running session B.

### Requirement: Exec shall be resource-capped

Every execution SHALL be bounded by a wall-clock timeout and by cgroup CPU and memory limits
sourced from configuration, so that one session cannot exhaust the shared worker's resources.

#### Scenario: A runaway command is killed

- **WHEN** a command exceeds its memory or time cap
- **THEN** nsjail/cgroups SHALL terminate it, and the gateway SHALL report the command as
  terminated (killed) rather than letting it consume the worker indefinitely.

### Requirement: Residual shared-kernel risk shall be documented

The system's threat model SHALL explicitly acknowledge that all sessions share one kernel and one
`computerd` process, and that isolation is therefore "strong practical sandboxing against
accidental leakage" rather than "defense against a dedicated hostile attacker"; this residual
risk SHALL be recorded for operators.

#### Scenario: Operator is aware of the isolation stance

- **WHEN** an operator reads the project documentation
- **THEN** they SHALL find an explicit statement that a kernel exploit or jail misconfiguration
  could cross sessions, and that the design assumes trusted (authenticated internal) users.
