# broker/resolve — terminal-unschedulability fast-fail

## ADDED Requirement: fast-fail on terminal unschedulability

The broker's `wait_for_ready` SHALL treat `PodScheduled=False
reason=Unschedulable` (from the Pod object or the Sandbox-status mirror) as
terminal and return 503 immediately, carrying the scheduler's condition
message. `SchedulingGated` SHALL NOT be treated as terminal.

### Scenario: unschedulable pod fails fast

- **WHEN** the pod reports `PodScheduled=False reason=Unschedulable` with a
  condition message while the claim timeout is far from expired
- **THEN** the resolve returns 503 within one poll tick with detail
  `sandbox <name> unschedulable: <message>` (message clipped to one line)

### Scenario: mirrored status fails fast too

- **WHEN** the Pod object is absent/unreadable but `Sandbox.status.conditions`
  mirrors `PodScheduled=False reason=Unschedulable`
- **THEN** the same immediate 503 with the mirrored condition message applies

### Scenario: gated pods keep waiting

- **WHEN** the pod reports `PodScheduled=False reason=SchedulingGated`
- **THEN** the poll continues to the configured claim timeout and the timeout
  503 carries the condition digest (gates can be lifted)

### Scenario: observability

- **WHEN** the fast-fail fires (or the wait times out)
- **THEN** `open_websandbox_broker_sandbox_not_ready_total` is incremented
  with `reason=unschedulable` (resp. `reason=timeout`), and the fast-fail
  logs a WARN with the sandbox name, reason, and condition message
