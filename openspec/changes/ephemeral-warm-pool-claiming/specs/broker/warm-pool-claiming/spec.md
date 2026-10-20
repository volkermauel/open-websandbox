# broker/warm-pool-claiming

## ADDED Requirement: ephemeral warm-pool claiming

When `BROKER_WARMPOOL` names a pool and the profile is `Ephemeral`, the
broker SHALL resolve through a `SandboxClaim` so the controller binds a
pre-warmed Sandbox in place.

### Scenario: warm bind

- **WHEN** a new ephemeral session resolves and the pool has a ready Sandbox
- **THEN** the broker creates a `SandboxClaim` (name `owui-e-<hex12>`,
  `warmPoolRef` set) with broker labels, reads the bound Sandbox name from
  `claim.status.sandbox.name`, and returns it once the pod is Ready —
  without waiting for a fresh pod boot

### Scenario: pool exhausted or disabled

- **WHEN** `BROKER_WARMPOOL` is empty, or the claim does not bind within the
  claim timeout (pool at 0 + controller cannot schedule)
- **THEN** the empty-pool case errors with a claim-timeout 503 naming the
  pool; the disabled case falls back to today's direct-Sandbox create

### Scenario: pool-key hop auth for ephemeral

- **WHEN** a broker→runtime call targets a Sandbox labeled
  `owui.io/auth=pool-key` and no per-session Secret exists
- **THEN** the broker authenticates with the `owui-pool-key` Secret; if
  neither key resolves, the call fails closed

### Scenario: reaper releases to the pool

- **WHEN** the reaper reaps an ephemeral Sandbox carrying the
  `owui.io/claim` annotation
- **THEN** it deletes the SandboxClaim (cascade removes Sandbox + pod; the
  pool replenishes), falling back to deleting the Sandbox when the claim is
  already gone

### Scenario: persistent untouched

- **WHEN** the profile is `Persistent`
- **THEN** resolution, per-session runtime keys, park/reap all behave
  exactly as before this change

### Scenario: chart defaults

- **WHEN** the chart renders with defaults
- **THEN** a dedicated pool template `code-standard-ephemeral-v1` (base
  template + pool-key volume), the `owui-pool-key` Secret, and a
  `SandboxWarmPool` referencing that template exist; the persistent template
  is unchanged
