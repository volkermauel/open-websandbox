# Specification: s3-tiered storage

## Requirement: third persistent backing mode gated behind a flag

The chart SHALL add `s3-tiered` to the `broker.persistentMode` enum and a new
`broker.s3` block. The entire feature SHALL be gated behind `broker.s3.enabled`
(default `false`), so a default `helm lint` / `helm template` / install is unchanged, and
the existing PVC modes (`per-user-pvc`, `shared-subpath`) SHALL be untouched.

### Scenario: default install is unchanged

- **WHEN** the chart is installed/templated with defaults (`broker.s3.enabled=false`)
- **THEN** no S3 credentials volume, no `BROKER_S3_*` env, and no s3-tiered behavior is
  rendered or activated; `helm lint` + `helm template` are clean and identical to pre-PR.

### Scenario: s3-tiered rendered when enabled

- **WHEN** `helm template --set broker.s3.enabled=true --set broker.persistentMode=s3-tiered`
- **THEN** the broker pod gains a projected S3-credentials Secret volume at `/etc/s3-creds`
  and `BROKER_S3_*` env; selecting `s3-tiered` without `s3.enabled=true` is rejected by the
  broker's fail-closed boot guard.

## Requirement: broker-orchestrated offload (no runtime→S3 path)

The broker SHALL be the sole S3 client (holding credentials from a projected Secret); the
runtime pod SHALL NEVER talk to S3 (the post-#50 network isolation is preserved).
Offload/restore SHALL drive two new runtime endpoints over the existing per-session auth
channel.

### Scenario: offload on reap

- **WHEN** an s3-tiered sandbox is reaped
- **THEN** the broker `GET /snapshot`s the running pod, uploads a single versioned object
  `users/<uid>/chats/<sid>/workspace-<ts>.tar.zst` (zstd-compressed tar), deletes prior
  objects under that prefix (keep-latest), and tags object-expiry metadata, BEFORE deleting
  the Sandbox CR and per-session key.

## Requirement: runtime snapshot/restore endpoints

The runtime SHALL expose `GET /snapshot` (stream a zstd-compressed tar of `/workspace`) and
`PUT /restore` (accept a zstd-compressed tar streamed into `/workspace`), both gated by the
SAME fail-closed per-session key as `/execute` / `/files/*`.

### Scenario: snapshot streams a zstd tar

- **WHEN** the broker calls `GET /snapshot` with a valid per-session Bearer
- **THEN** the response body is a zstd-compressed tar of `/workspace` that round-trips
  through `PUT /restore`; `/` and `/metrics` remain open.

### Scenario: size limit is enforced (no truncation)

- **WHEN** the logical `/workspace` size exceeds `MAX_WORKSPACE_BYTES` (snapshot) or the
  incoming restore stream exceeds it
- **THEN** the endpoint refuses with 413 (fail-on-exceed) and does NOT truncate.

## Requirement: synchronous restore on resume (D4)

The broker SHALL restore S3 → the new pod AFTER pod readiness and BEFORE marking the
session ready, so resumed data is present.

### Scenario: restore failure fails the resume

- **WHEN** an s3-tiered session resumes and an object exists but `PUT /restore` fails
- **THEN** the resume is failed (error surfaced); the broker does NOT hand back an empty
  workspace.

### Scenario: first creation with no snapshot

- **WHEN** an s3-tiered chat is created for the first time (no object under its prefix)
- **THEN** restore is a no-op (the pod starts empty) and the session becomes ready.

## Requirement: failure semantics (D7)

Offload failure on reap SHALL retry with backoff and keep the pod+CR alive until success
or max-attempts (no silent data loss). Restore failure on resume SHALL fail the resume.

### Scenario: offload retry + keep-alive

- **WHEN** offload fails on reap
- **THEN** the broker retries with backoff up to a max-attempts ceiling and, on exhaustion,
  leaves the Sandbox CR + key intact (re-tried next reaper tick) rather than deleting it.

## Requirement: periodic sync (R1)

The broker SHALL run a leader-gated background task that, every
`broker.s3.periodicSyncInterval`, snapshots every Running s3-tiered sandbox to S3, so a
node loss mid-session loses at most one interval. The on-reap offload remains the final
authoritative snapshot.

### Scenario: leader-gated periodic snapshot

- **WHEN** the leader broker's periodic-sync tick fires
- **THEN** each Running s3-tiered sandbox is snapshotted to S3 under its prefix; a non-leader
  replica does not run the task.

## Requirement: per-session retention (R2 / D5)

Object retention SHALL be session-scoped: the broker SHALL set per-object expiry metadata
at offload and keep only the latest object per session prefix; operators SHALL apply a
documented bucket lifecycle rule on the `users/<uid>/chats/<sid>/` prefix as the portable
age-out safety net.

### Scenario: expiry metadata + keep-latest

- **WHEN** the broker offloads a snapshot
- **THEN** it sets the object `Expires` (now + retentionDays) + session metadata and deletes
  any prior object under the same prefix, so a prefix holds exactly one object.

## Requirement: hot-tier volume + compression/encryption (D2 / D9)

The s3-tiered `/workspace` SHALL be a size-limited `emptyDir` (default) or `tmpfs` (opt-in),
default `sizeLimit` 2Gi. Snapshots SHALL be zstd-compressed; TLS in transit + SSE-S3 at rest.

### Scenario: size-limited hot tier

- **WHEN** an s3-tiered Sandbox is created
- **THEN** its `/workspace` is an `emptyDir` with `sizeLimit` (2Gi default) — or `medium:
  Memory` when `tmpfs` is enabled — independent of any PVC.
