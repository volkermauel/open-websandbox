## ADDED Requirements

### Requirement: Sessions shall be created on demand and reaped when idle

The gateway SHALL create a session's VFS subtree lazily on first use and SHALL track the last
activity time of each session. A background sweeper SHALL evict sessions that have been idle
longer than `SESSION_IDLE_TIMEOUT` (default 1800 seconds), reclaiming their resources.

#### Scenario: A session is created on first touch

- **WHEN** a (user, chat) pair issues its first request
- **THEN** the gateway SHALL create the corresponding VFS subtree if it does not exist and SHALL
  record the session in its lifecycle table.

#### Scenario: An idle session is reaped

- **WHEN** a session has had no activity for longer than `SESSION_IDLE_TIMEOUT`
- **THEN** the sweeper SHALL reap it (archiving or deleting its subtree per configuration) and
  free its slot against the session caps.

### Requirement: In-use sessions shall never be evicted

The gateway SHALL NOT evict a session that has any active execution or open output stream, even
under cap pressure; eviction SHALL consider only sessions with zero active executions and no open
streams.

#### Scenario: A session with a running command is preserved

- **WHEN** the global session cap is reached and the oldest session has a command still running
- **THEN** the gateway SHALL select a different idle session for eviction, or return HTTP 503 if
  no idle session is available, and SHALL NOT kill the running command.

### Requirement: The control plane shall survive restart

The gateway SHALL persist session metadata in a table within the dofs database (`sessions_meta`)
and, on restart, SHALL reopen the persistent database, re-establish the `computerd` connection,
and rebuild its in-memory session index from that table, so that sessions resume without data
loss. In-flight executions at the moment of restart are not guaranteed to survive; clients SHALL
be expected to retry.

#### Scenario: Sessions resume after a gateway restart

- **WHEN** the gateway container restarts
- **THEN** previously created session subtrees SHALL remain accessible from their existing
  (user, chat) keys, because the gateway rebuilt its index from `sessions_meta` on startup.

### Requirement: Eviction policy shall evict oldest-idle first under caps

When a new session would exceed the global or per-user cap, the gateway SHALL evict the oldest
idle eligible session of the relevant scope before creating the new one, preserving fairness and
bounded resource use.

#### Scenario: Oldest idle session is evicted to make room

- **WHEN** creating a new session would exceed `MAX_SESSIONS` and idle sessions exist
- **THEN** the gateway SHALL evict the single oldest idle session and then create the new one.
