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
