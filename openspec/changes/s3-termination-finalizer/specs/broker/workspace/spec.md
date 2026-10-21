# broker/workspace — termination-ordered S3 snapshot

## ADDED Requirements

### Requirement: deletion waits for the S3 snapshot

In `s3-record` mode, a managed Sandbox SHALL carry the
`owui.dev/s3-snapshot` finalizer. A Sandbox with `metadata.deletionTimestamp`
SHALL NOT have its finalizer removed until the chat workspace is durably
stored in S3.

#### Scenario: out-of-band delete still archives

- **WHEN** a managed Sandbox is deleted by anything other than the broker's
  release path (manual delete, controller rollout, eviction of the claim)
- **THEN** the broker observes `deletionTimestamp`, offloads `/workspace` via
  the runtime API while the pod still serves, and removes the finalizer only
  after the S3 put completes

#### Scenario: fresh snapshot short-circuits

- **WHEN** the reaper already archived the chat at its current snapshot
  generation before release
- **THEN** the finalizer is removed without a second offload and the delete
  completes in the reaper's existing release timing

#### Scenario: broker outage during delete

- **WHEN** the broker is unavailable while a Sandbox is Terminating
- **THEN** the Sandbox remains in Terminating with its pod serving, and the
  offload plus finalizer removal complete after the broker recovers

#### Scenario: offload failure holds deletion

- **WHEN** the S3 put fails (storage backend unreachable)
- **THEN** the finalizer stays, the broker retries with backoff, and an
  alert-grade log records the blocked deletion

### Requirement: unresumable suspended sandbox release

In `s3-record` mode, a Suspended Sandbox whose offload fails SHALL be
released only when the broker re-fetches it and still finds
`operatingMode: Suspended`. That degraded release SHALL skip the S3
snapshot and SHALL be surfaced via a dedicated stat and an alert-grade log.
A Sandbox the offloader already resumed SHALL NOT be released: the ordinary
offload-failure keep-alive applies.

#### Scenario: still-suspended candidate released without a snapshot

- **WHEN** the reaper reaps a Suspended `s3-record` Sandbox past the park
  window, its offload fails, and a re-fetch still reports `operatingMode:
  Suspended`
- **THEN** the broker releases the Sandbox (deleting its bound claim, when
  present, so the cascade frees the pod) without an S3 snapshot, and the
  release is surfaced via the `suspended_offload_unavailable` stat plus an
  alert-grade log

#### Scenario: resumed candidate keeps the keep-alive

- **WHEN** the offloader has already resumed the Sandbox (patched
  `operatingMode: Running` and booted a serving pod) before its S3 put
  failed, so the broker's re-fetch no longer reports `Suspended`
- **THEN** the broker counts an ordinary offload failure, keeps the Sandbox
  alive for the next tick, and removes no finalizer

#### Scenario: S3 outage surfaces as offload unavailability

- **WHEN** the S3 backend is unreachable for a Suspended `s3-record` Sandbox
  whose offload cannot complete, and the Sandbox remains `Suspended`
- **THEN** the release proceeds without a snapshot and the same
  `suspended_offload_unavailable` stat (not a pod-absence stat) is
  incremented, so alerting pages on offload availability rather than pod
  absence
### Requirement: drain-belt for direct pod eviction

The `s3-record` SandboxTemplate pod spec SHALL set
`terminationGracePeriodSeconds: 45` and a `preStop` exec hook that sleeps 30
seconds, so a kubelet-initiated pod eviction (node drain) keeps the runtime
serving long enough for the broker's deletion watch to archive the
workspace.

#### Scenario: node drain archives via the belt path

- **WHEN** a node hosting a serving sandbox pod is drained
- **THEN** the pod's preStop delay gives the broker's watch time to offload
  the workspace before SIGTERM, and the S3 object for the chat is current

#### Scenario: belt is bounded

- **WHEN** the offload cannot complete within the preStop window
- **THEN** the pod terminates when the hook expires, the loss is logged by
  the broker (missing expected generation), and deletion of the Sandbox
  still completes normally
