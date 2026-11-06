# broker/sandbox-lifecycle — template-drift self-heal

## ADDED Requirement: template-hash stamp

Every broker-created Sandbox SHALL carry an `owui.io/template-hash`
annotation: sha256 (hex) over a canonical (key-order-free) JSON encoding of
the SandboxTemplate fields cloned into the Sandbox (today `spec.podTemplate`),
computed from the pristine template BEFORE the clone is mutated.

### Scenario: stamp at creation

- **WHEN** a Sandbox is created via resolve's cold-create path or the explicit
  POST `/api/sandboxes` handler
- **THEN** its annotation equals `template_hash(&template)` of the live
  template it was cloned from

## ADDED Requirement: drift recycle (idle only)

A Sandbox whose stamp differs from the live base-template hash SHALL be
recycled — deleted and recreated through the shared cold-create path — but
ONLY when the reaper's idle predicate says nothing is using it. The leader
reaper SHALL perform the same recycle for idle drifted sandboxes (the main
path, since idle sandboxes receive no requests).

### Scenario: idle drifted sandbox is recycled

- **WHEN** a sandbox carries an older hash than the live template, its idle
  time passes the reaper's threshold (`decide()` ≠ Skip), it is not
  `Suspended`, not terminating, and outside the 120s `broker-recycled-at`
  grace window
- **THEN** it is deleted and recreated from the CURRENT template under the
  same deterministic name (fresh hash + grace stamp), logged as
  `recycled sandbox <name>: template <old> -> <new>`

### Scenario: active drifted sandbox keeps serving

- **WHEN** a drifted sandbox has fresh `broker-last-used` (active connections
  / recent resolves) or is parked (`Suspended`)
- **THEN** resolve logs the drift at debug and keeps serving it; the reaper
  recycles it once idle

### Scenario: no false recycling

- **WHEN** the sandbox has no hash stamp (legacy/controller-built), the
  template cannot be read, or the pre-delete offload fails (D7)
- **THEN** the sandbox is kept serving (debug/warn log; the reaper retries the
  recycle on a later tick), and a sandbox recycled within the grace window is
  never recycled again immediately (no loops)
