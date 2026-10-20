# broker/workspace — S3 store-of-record

## MODIFIED Requirement: chat workspace storage model

With `BROKER_PERSISTENT_MODE=s3-record` (default after this change), S3 SHALL
be the durable store of record for every persistent chat workspace. Sandboxes
mount only a per-sandbox scratch PVC from the pool template's
`volumeClaimTemplates`; no broker-managed per-user PVC exists.

### Scenario: suspend releases everything k8s-side

- **WHEN** a persistent chat goes idle past `parkIdleSeconds`
- **THEN** the broker snapshots `/workspace` to `users/<u>/chats/<c>/latest.zst`
  via the runtime API, deletes the claim, and the ownerRef chain garbage-
  collects the Sandbox, Pod, and scratch PVC — no broker-created PVC remains

### Scenario: resume restores before readiness

- **WHEN** a resolve finds an S3 snapshot for the chat and no live sandbox
- **THEN** a claim binds (warm pool), the snapshot is restored into the fresh
  scratch PVC, and readiness is not reported until the restore completes

### Scenario: scratch PVC lifecycle is controller-owned

- **WHEN** any Sandbox is deleted (reap, suspend, manual)
- **THEN** its `<templateName>-<sandboxName>` scratch PVC is garbage-collected
  by the ownerRef chain without broker involvement

### Scenario: draft adoption is an S3 copy

- **WHEN** a new chat adopts the user's draft workspace
- **THEN** the broker performs an S3 server-side copy from the draft prefix
  to the chat prefix (no PVC-mounting move Job)

### Scenario: retention replaces k8s cleanup

- **WHEN** a chat prefix's objects age past the configured S3 lifecycle rule
- **THEN** the storage backend expires them; the platform performs no
  additional bookkeeping

### Scenario: legacy transition

- **WHEN** `BROKER_PERSISTENT_MODE=per-user-pvc` (transition value)
- **THEN** the v0.2.x per-user-PVC behavior is available unchanged, and
  lazy per-chat migration to s3-record runs on first resolve

### Scenario: ephemeral profile untouched

- **WHEN** the profile is `Ephemeral`
- **THEN** emptyDir scratch + warm claiming behave exactly as shipped in
  v0.2.1 (no S3, no VCT requirement)
