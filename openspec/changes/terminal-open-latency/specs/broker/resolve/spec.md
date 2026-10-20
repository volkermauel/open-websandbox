# broker/resolve — pod-direct readiness

## ADDED Requirement: pod-direct readiness

The broker's `wait_for_ready` SHALL treat a sandbox's Pod object (same name,
runtime namespace) as the primary readiness signal, checking it before the
Sandbox status on every poll tick.

### Scenario: pod Ready before Sandbox status

- **WHEN** the pod named `<sandbox>` has `status.conditions[Ready].status ==
  "True"` and a non-empty `status.podIP`, while `Sandbox.status` does not yet
  report Ready
- **THEN** the resolve returns immediately with that pod IP (after one
  best-effort sandbox GET for the S3 last-key annotation)

### Scenario: pod absent or not ready

- **WHEN** the pod GET returns NotFound, or the pod exists but is not Ready
- **THEN** the poll continues on the pre-existing Sandbox-status path
  unchanged (backward-compatible fallback for controller naming changes)

### Scenario: timeout diagnostics

- **WHEN** the claim timeout expires and a pod object was observed
- **THEN** the 503 digest includes pod-level facts (phase, `PodScheduled`
  reason such as `Unschedulable`) alongside any Sandbox conditions

### Scenario: runtime manifest defaults

- **WHEN** the chart or `deploy/base` is applied without overrides
- **THEN** the runtime readiness probe has `periodSeconds: 1` and
  `BROKER_PARK_IDLE_SECONDS` is `900`
