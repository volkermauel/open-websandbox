# S3-tiered ephemeral storage mode

Issue: #52. Adds a third **persistent backing mode** `s3-tiered` (alongside
`per-user-pvc` / `shared-subpath`) that eliminates PVCs: hot tier = ephemeral pod
storage (`emptyDir`/`tmpfs`, size-limited); cold tier = S3. On session reap the broker
offloads `/workspace` → S3; on resume it restores S3 → the new ephemeral pod.
Fully behind `broker.s3.enabled` (default OFF) + `broker.persistentMode: s3-tiered` —
additive, non-breaking, default e2e unaffected.

## Why

The two existing persistent modes both bind a `PersistentVolumeClaim` to every chat
(RWX cephfs). PVCs are the single heaviest operational dependency of the platform: a
dedicated RWX StorageClass, capacity/quota planning, slow reclaims, and a node-disk
footprint per idle chat. On providers where RWX is unavailable or expensive (e.g.
Proxmox, single-replica object stores, edge clusters), a PVC-per-chat is a blocker.

`agent-sandbox` sandboxes are already network-isolated (#43/#50: per-session broker↔runtime
key + default-deny NetworkPolicy). Issue #52 asks for a PVC-free persistence mode that
keeps that isolation intact: the runtime pod NEVER talks to S3 — the **broker** holds the
S3 client + credentials and drives offload/restore over the existing per-session auth
channel, using two new runtime endpoints. The cold tier is a bring-your-own
S3-compatible bucket (AWS S3 / MinIO / Cloudflare R2 / Proxmox S3 API).

## Proposal

A new `broker.persistentMode: s3-tiered` in which a persistent-profile sandbox's
`/workspace` is a size-limited `emptyDir` (optionally `tmpfs`) instead of a PVC. The
broker is the sole S3 client (D1): on reap it `GET /snapshot`s the running pod and
uploads a versioned `workspace-<ts>.tar.zst`; on resume it downloads the latest object
and `PUT /restore`s it into the freshly-booted pod. A leader-gated periodic-sync task
snapshots long-running sessions so a node loss mid-session loses at most one interval
(R1). Object retention is session-scoped via per-object expiry metadata + a recommended
bucket lifecycle rule (R2). Everything is gated behind `broker.s3.enabled` (default OFF);
with it off, the chart, broker and runtime behave exactly as today.

### Surfaces

- **`runtime/server.py`** — two new auth-gated endpoints, same per-session `_auth_runtime`
  as `/execute`/`/files/*`:
  - `GET /snapshot` → streams `tar -cf - -C /workspace . | zstd -` (zstd-compressed tar).
    Pre-checks the logical `/workspace` size against `MAX_WORKSPACE_BYTES` (D9
    fail-on-exceed, 413 before streaming — no truncation).
  - `PUT /restore` → reads the request body stream, enforces `MAX_WORKSPACE_BYTES` on the
    incoming compressed bytes (413 on exceed), and pipes `zstd -d | tar -xf - -C /workspace`.
  Both reuse the fail-closed per-session key (the broker attaches it via
  `_runtime_auth_headers`). `/` (health) + `/metrics` stay open.
- **`broker/main.py`** — `aioboto3` S3 client (soft-imported like OTel; created lazily,
  credentials read from a projected Secret volume at `/etc/s3-creds/{access-key-id,
  secret-access-key}` so no secret lands in pod env — consistent with #48). Two
  orchestrators:
  - `_offload_to_s3(sandbox, pod_ip, user_id, session_id)` = `GET /snapshot` (stream) →
    multipart `PutObject` `users/<uid>/chats/<sid>/workspace-<ts>.tar.zst`, deleting prior
    objects under the prefix (keep-latest), tagging object expiry metadata (R2). Hooked
    into the reaper's reap path for s3-tiered sandboxes BEFORE CR/key deletion, with
    retry+backoff + keep-alive on failure (D7).
  - `_restore_from_s3(...)` = list prefix → latest `workspace-<ts>.tar.zst` → `PUT /restore`
    (stream). Hooked into `resolve_sandbox` AFTER pod readiness, BEFORE returning
    (D4 sync). No-op when no object exists (first creation); fails the resume (raises) when
    an object exists but restore fails (D7 — never start empty).
  - `_create_sandbox`: for `s3-tiered`, keep the base template's `emptyDir` `/workspace`
    but override `sizeLimit` (default 2Gi) + `medium: Memory` when `tmpfs` is enabled (D2),
    and label `broker-persistent-mode: s3-tiered` so the reaper treats it as
    offload-then-reap-at-IDLE_TTL (like ephemeral, since the cold tier is S3).
  - Periodic-sync scheduler (R1): a leader-gated background task (started/stopped in
    `_apply_leadership` alongside the reaper, only when `S3_ENABLED`) that every
    `BROKER_S3_PERIODIC_SYNC_SECONDS` snapshots every Running s3-tiered sandbox via
    `_offload_to_s3`.
  - `_validate_config`: fail-closed boot guard — refuse to start when
    `persistentMode=s3-tiered` but `broker.s3.enabled=false` (misconfiguration).
- **Chart** — `broker.s3.{enabled(false), endpoint, bucket, prefix, credentialsSecret,
  retentionDays, compression(zstd), periodicSyncInterval, sizeLimit(2Gi), tmpfs(false)}`;
  `persistentMode` enum gains `s3-tiered`; a projected volume mounts the named S3-creds
  Secret into the broker at `/etc/s3-creds` + `BROKER_S3_*` env, all gated on
  `broker.s3.enabled`. The chart does NOT create the creds Secret (bring-your-own, D6).
- **Tests** — runtime endpoint unit tests (round-trip snapshot→restore on a tmp workspace,
  size-limit fail-on-exceed, auth gating); broker unit tests with a fake in-memory S3
  client (offload, restore, periodic-sync, retention-expiry metadata, D7 keep-alive on
  offload failure, D4 sync-blocks-readiness / restore failure fails resume). e2e:
  default path unchanged (s3 off); an optional MinIO-in-cluster e2e is a scoped follow-up
  (unit coverage is thorough).

## Owner decisions (issue #52 — implemented exactly)

- **D1 — Broker-orchestrated.** Broker holds the S3 client + creds; the runtime pod NEVER
  talks to S3 (preserves the post-#50 network isolation). Two new runtime endpoints over
  the existing per-session auth channel: `GET /snapshot`, `PUT /restore`.
- **D2 — Hot tier:** `emptyDir` (default), with a `tmpfs` option, both size-limited
  (`sizeLimit`, default 2Gi). The broker realizes this as the `/workspace` volume for the
  s3-tiered Sandbox variant (overriding the base template's emptyDir at sandbox creation,
  mirroring how the PVC modes already override the workspace volume broker-side).
- **D3 — Object format:** one compressed tarball per session per reap:
  `users/<uid>/chats/<sid>/workspace-<ts>.tar.zst` (versioned, atomic — zero-padded `ts`
  so lexical order = chronological). Plus R1 (periodic sync).
- **D4 — Restore timing:** synchronous — block session readiness until restore completes.
- **D5 — Retention:** per-session lifetime (R2) via per-object expiry metadata set at
  offload + a recommended bucket lifecycle rule on the `users/<uid>/chats/<sid>/` prefix
  (operator-managed; the broker does NOT auto-manage lifecycle to stay portable across
  AWS/MinIO/R2/Proxmox). The broker keeps only the latest object per prefix (deletes
  prior) to bound storage.
- **D6 — Provider + auth:** bring-your-own S3-compatible endpoint. Auth = static access
  key in a k8s Secret (projected into the broker). `aioboto3` (added to
  `broker/requirements.in`).
- **D7 — Failure semantics:** offload fails on reap → retry with backoff, keep pod+CR
  alive until success or max-attempts (no silent data loss). Restore fails on resume →
  fail the resume (surface error), do NOT start empty.
- **D8 — Coexistence:** alongside the PVC modes (PVC modes untouched).
- **D9 — Size/compression/encryption:** 2Gi `sizeLimit` default, fail-on-exceed (no
  truncation), zstd compression, TLS in transit + SSE-S3 at rest.

## Non-goals

- Changing the vendored upstream controller / CRDs / namespaces (byte-for-byte preserved).
- Touching the PVC modes (`per-user-pvc`, `shared-subpath`) or the default e2e path.
- **R3 (PVC + `reclaimPolicy:Delete` + S3 hybrid)** is a stretch goal, deferred to a
  follow-up issue. Core `s3-tiered` is the scope of this change.
- A chart-managed bucket lifecycle (portability risk across providers); operators apply the
  documented rule. The broker sets per-object expiry metadata + keeps-latest as the
  in-band mechanism.

## Decisions

- **D-impl-1 Single base SandboxTemplate.** The s3-tiered `/workspace` variant is realized
  broker-side (override the base template's `emptyDir` with `sizeLimit`/`medium` at
  sandbox creation), mirroring how the PVC modes already override the workspace volume
  broker-side. No second SandboxTemplate CRD object is rendered — the broker is the single
  chokepoint that already clones + specializes the base template per profile.
- **D-impl-2 Streaming, not buffering.** Offload streams `GET /snapshot` → S3 multipart
  `PutObject`; restore streams S3 `GetObject` → `PUT /restore`. Nothing is buffered whole
  in broker memory (the broker's memory limit is 512Mi; a 2Gi workspace would OOM it).
- **D-impl-3 Soft-import aioboto3.** Like OTel: `try: import aioboto3 except ImportError:
  aioboto3 = None`. All S3 code is guarded behind `S3_ENABLED`; with s3 off (default) the
  broker imports + boots without aioboto3 installed (unit tests run without it).
- **D-impl-4 Lifecycle.** s3-tiered sandboxes reap at `IDLE_TTL` (like ephemeral) after a
  successful offload, since the cold tier is S3 — there is no PVC to park. The periodic
  sync covers the active window so node loss loses ≤ one `periodicSyncInterval`.
