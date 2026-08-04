## ADDED Requirements

### Requirement: One gVisor sandbox per active session

The system SHALL allocate at most one sandbox per active agent session, not one
per registered user. A sandbox SHALL be claimed for the duration of a session and
destroyed when the session ends; the warm pool SHALL then build a clean
replacement. No claimed sandbox in which user code has run SHALL be reassigned to
another session.

#### Scenario: Session end destroys the sandbox

- **WHEN** a session is deleted
- **THEN** its claimed sandbox and session-specific storage/secrets SHALL be
  removed, and the warm pool SHALL replenish with a fresh, clean sandbox.

### Requirement: Users shall never receive Kubernetes credentials

End users and agents SHALL NOT receive a kubeconfig, Kubernetes token, or
permission to create `SandboxClaim` resources directly. Only the broker SHALL
create/watch/delete claims, using a narrowly scoped service account.

#### Scenario: A user cannot reach the sandbox directly

- **WHEN** a user attempts to contact the router or Kubernetes API bypassing the
  broker
- **THEN** the request SHALL be denied (network policy / RBAC), and the
  user-supplied routing identifiers SHALL be ignored in favour of server-side
  resolution.

### Requirement: Every sandbox shall be gVisor-isolated and hardened

Every sandbox pod SHALL use `runtimeClassName: gvisor`, run as a non-root UID
with all Linux capabilities dropped, `hostNetwork`/`hostPID`/`hostIPC` false, no
hostPath/hostPort, and SHALL have CPU and memory limits on every container.

#### Scenario: A sandbox cannot escalate to the host

- **WHEN** code inside a sandbox attempts a privileged operation or host access
- **THEN** gVisor and the dropped-capability/no-host* pod context SHALL prevent
  it, and the ValidatingAdmissionPolicy SHALL have rejected any manifest weakening
  these controls.

### Requirement: Sandbox networking shall be default-deny

The runtime namespace SHALL default-deny ingress and egress. A sandbox SHALL be
reachable only from the router (itself reachable only from the broker), and SHALL
egress only to DNS, the internal object store, and an explicit policy-controlled
proxy.

#### Scenario: A sandbox cannot reach the management network

- **WHEN** a sandbox attempts to reach the Kubernetes API, other internal
  services, or the public internet directly
- **THEN** the connection SHALL be denied by NetworkPolicy.

### Requirement: Production images shall be pinned by digest

Every container image deployed to production SHALL be pinned by immutable digest;
mutable tags (including `latest` / `latest-main`) SHALL NOT appear in production
manifests. The upstream `agent-sandbox` manifest and gVisor release SHALL be
vendored with recorded checksums.

#### Scenario: A mutable tag is rejected in production

- **WHEN** a production manifest references a mutable image tag
- **THEN** admission SHALL reject it.

### Requirement: The broker shall recover statelessly from restart

The broker SHALL NOT require an external database for session state in v1. On
restart it SHALL rebuild active session state from the broker-owned
`SandboxClaim` resources (labelled `sandbox.open-websandbox.local/created-by=broker`),
and session tokens SHALL be signed and opaque so ownership can be re-verified.

#### Scenario: Broker restart does not orphan sessions

- **WHEN** the broker restarts
- **THEN** it SHALL re-list its claims, re-derive session ownership, and continue
  serving or cleanly expiring them without creating duplicate or cross-user
  assignments.
