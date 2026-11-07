# broker/reaper — termination finalizer contract

## MODIFIED Requirement: termination offload (deletionTimestamp branch)

A Terminating sandbox carrying the `owui.dev/s3-snapshot` finalizer SHALL
have the finalizer released in the same tick when it has NO pod IP and is
NOT `Suspended` — there is nothing to offload, and holding would block the
deletion forever. Each such release SHALL emit a WARN naming the sandbox and
the reason, increment
`open_websandbox_broker_termination_released_without_offload_total`, and
count `ReaperStats.termination_released_no_offload`.

The release condition mirrors the offloader's own first check
(`S3Offload::ensure_running_pod`: status pod IP, else resume-if-Suspended):
if the sandbox has no pod IP and is not Suspended, the offload fails with
the same "no pod IP" error on every tick, so the hold can never converge and
no snapshot can exist to lose. Two cases MUST still hold the finalizer
(D7):

- a pod IP exists but the offload call fails (transient — retried next
  tick),
- the sandbox is `Suspended` (the offloader may resume it to snapshot; the
  #207 contract is unchanged).

### Scenario: terminating sandbox that never scheduled is released

- **WHEN** a terminating sandbox carries the finalizer, has no pod IP
  (never scheduled, or the pod is long gone) and is not `Suspended`
- **THEN** the offload is not attempted, the finalizer is released, a WARN
  names the sandbox and states the release reason, and
  `termination_released_no_offload` + the new counter increment

### Scenario: pod IP present but offload failing still holds

- **WHEN** a terminating sandbox carries the finalizer, has a pod IP, and
  the offload call fails
- **THEN** the finalizer stays held, `termination_blocked` increments, and
  the sandbox remains for the next tick's retry

### Scenario: suspended terminating sandbox keeps the resume contract

- **WHEN** a terminating sandbox carries the finalizer, is `Suspended` and
  has no pod IP
- **THEN** the no-pod release does not fire; the offload (which may resume
  the sandbox) is attempted and its failure holds as before
