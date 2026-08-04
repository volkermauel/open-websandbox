## ADDED Requirements

### Requirement: All requests shall present a valid shared bearer token

The gateway SHALL validate an `Authorization: Bearer <OPEN_TERMINAL_API_KEY>` token on every
authenticated route using a constant-time comparison, and SHALL reject requests with a missing or
mismatched token with HTTP 401. The token source SHALL be the `OPEN_TERMINAL_API_KEY` environment
variable or its `_FILE` (Docker-secret) variant.

#### Scenario: A request without a valid token is rejected

- **WHEN** a client calls `/execute` without a bearer token or with a wrong one
- **THEN** the gateway SHALL return HTTP 401 and SHALL not execute any command.

### Requirement: X-User-Id shall be required and X-Session-Id optional

The gateway SHALL require an `X-User-Id` header on every authenticated request (HTTP 400 if
absent) and SHALL treat `X-Session-Id` as optional, falling back to a per-user default session
when it is absent rather than rejecting the request.

#### Scenario: Missing user identity is rejected

- **WHEN** an authenticated request lacks `X-User-Id`
- **THEN** the gateway SHALL return HTTP 400.

#### Scenario: Missing chat identity falls back gracefully

- **WHEN** an authenticated request has `X-User-Id` but no `X-Session-Id`
- **THEN** the gateway SHALL route the request to a per-user default session subtree instead of
  failing.

### Requirement: Session keys shall be hashed and path-sanitised

The user identity SHALL be mapped to a stable hash (`sha256(X-User-Id)[:12]`) for use in object
names and paths, and the chat identifier SHALL be sanitised to a single safe path component
matching `[A-Za-z0-9._-]{1,64}`, rejecting any value containing `/`, `..`, or a NUL byte, so that
no session can reference another session's subtree by path traversal.

#### Scenario: A traversal attempt is rejected

- **WHEN** a client sends `X-Session-Id` equal to `../otheruser`
- **THEN** the gateway SHALL reject the value and SHALL NOT resolve a subtree outside the
  requesting user's namespace.

### Requirement: Per-user session caps shall be enforced

The gateway SHALL enforce a configurable maximum number of concurrent sessions per user
(`MAX_SESSIONS_PER_USER`) in addition to a global cap (`MAX_SESSIONS`), evicting the oldest idle
session of the relevant user when the cap is exceeded.

#### Scenario: A user exceeding their cap evicts their oldest idle session

- **WHEN** a user opens more sessions than `MAX_SESSIONS_PER_USER`
- **THEN** the gateway SHALL evict that user's oldest idle session, and SHALL NOT evict any
  session belonging to a different user.
