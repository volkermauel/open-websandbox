## ADDED Requirements

### Requirement: One gVisor sandbox per active session

The system SHALL allocate at most one sandbox per active agent session, not one
per registered user. A sandbox SHALL be claimed for the duration of a session and
destroyed when an ephemeral session ends; the warm pool SHALL then build a clean
replacement. No claimed sandbox in which user code has run SHALL be reassigned to
another session.

#### Scenario: Ephemeral session end destroys the sandbox

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
reachable only from the router (itself reachable only from the broker). Egress SHALL
be limited to DNS plus HTTPS (TCP/443) to the public internet — required for dynamic
package installation — EXCLUDING the cluster pod, service, and host-network CIDRs so
a sandbox cannot reach internal services; this open-443 egress SHALL be replaced by a
policy-controlled domain-allowlisting egress proxy in a later phase.

#### Scenario: A sandbox cannot reach the management network

- **WHEN** a sandbox attempts to reach the Kubernetes API, other internal services,
  or any non-HTTPS (non-443) destination on the public internet
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

### Requirement: The broker shall reuse one sandbox across all calls in a session

For a given session the broker SHALL create or claim a sandbox on first use and route
every subsequent tool call in that session to the SAME sandbox; it SHALL NOT create
or destroy a sandbox per tool call, to avoid churn, cold-start latency, and orphaned
pods ("space left behind").

#### Scenario: Repeated calls in one session hit one sandbox

- **WHEN** an agent issues N tool calls within one session
- **THEN** at most one sandbox SHALL have been created/claimed for that session, and
  files written by an earlier call SHALL be visible to a later call in the same
  session.

### Requirement: Sandboxes shall support dynamic dependency installation

A sandbox SHALL allow the agent to install packages at runtime via `pip` and `npm`,
including packages with native extensions (so the image SHALL ship a compiler
toolchain, `python3-dev`, `nodejs`/`npm`, and writable per-session cache and prefix
directories owned by the non-root user). Installation SHALL succeed against the
public registries over the egress above.

#### Scenario: An agent installs a package with a native extension

- **WHEN** an agent runs `pip install` (or `npm install`) for a package requiring
  compilation
- **THEN** the install SHALL succeed and the import/require SHALL work for the
  remainder of the session.

### Requirement: The runtime image shall ship commonly-used libraries pre-installed

The `code-standard` image SHALL pre-install a curated, version-controlled set of
frequently-used libraries (e.g. PyYAML, numpy, pandas, openpyxl, requests,
beautifulsoup4, lxml) so a sandbox claimed from the warm pool is immediately
productive without a first-call install penalty.

#### Scenario: A warm sandbox has common libraries on first use

- **WHEN** a sandbox is claimed from the warm pool and an agent imports a curated
  library without installing it
- **THEN** the import SHALL succeed with no network round-trip.

### Requirement: A session may optionally persist files across sessions

The system SHALL offer a per-session persistence profile. **Ephemeral** (default)
mounts an emptyDir at `/workspace` and destroys all files when the sandbox is
terminated. **Persistent** (opt-in) uses a `SandboxTemplate` with
`volumeClaimTemplates` (StatefulSet semantics) to bind a dedicated PVC at
`/workspace`; the controller SHALL reattach the same PVC if the pod is recreated.
For the persistent profile the broker SHALL NOT terminate the sandbox when the
logical session ends — it SHALL retain it and record its `claim_name` keyed by user,
so a later session for that user SHALL re-acquire the same sandbox (and its files) by
`claim_name`. The broker SHALL terminate (releasing the PVC) any persistent sandbox
unused longer than a configurable retention TTL.

#### Scenario: A user resumes a persistent session after it ended

- **WHEN** a user ends a persistent session and starts a new one later, within the
  retention TTL
- **THEN** the new session SHALL re-acquire the prior sandbox by `claim_name`, its
  `/workspace` SHALL contain the files and installed dependencies from the prior
  session, and no other user's files SHALL be visible.
