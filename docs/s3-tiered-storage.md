# S3 cold-tier storage

`broker.s3.enabled` turns on the **cold tier** — an S3-compatible bucket holding
compressed workspace snapshots. Since #142 the cold tier is **independent of the
hot tier** (`broker.persistentMode`), so it composes with all of them:

| hot tier (`persistentMode`) | behavior with S3 on |
|---|---|
| `empty-dir` | tier-only (#52 behavior): reap offloads `/workspace` → S3 and deletes the pod; resume restores into the fresh pod. No PVCs at all. |
| `per-user-pvc` / `shared-subpath` | **hybrid tiering**: park/resume serves the PVC directly (no S3 I/O); reap offloads to S3, then purges the chat dir from the PVC to free the hot tier; the next resolve restores from S3. |

On session reap the broker offloads `/workspace` → S3; on resolve it restores S3 →
`/workspace`. Fully behind `broker.s3.enabled` (default **off**), so the default
install is unchanged.

## How it works

- **Hot tier** — a size-limited `emptyDir` `/workspace` (default `sizeLimit: 2Gi`, optional
  `tmpfs`). No PVC is bound to the chat.
- **Cold tier** — one zstd-compressed tarball per session per offload:
  `users/<uid-hash>/chats/<sid-hash>/workspace-<ts>.tar.zst` (versioned, atomic).
- **Broker-orchestrated** — the broker is the **sole S3 client**; the runtime pod **never**
  talks to S3 (the post-#50 network isolation is preserved). The broker drives offload/
  restore over the existing per-session-key auth channel via two runtime endpoints:
  `GET /snapshot` (stream `tar | zstd`) and `PUT /restore` (`zstd -d | tar -x`).

### Lifecycle

| event | behavior |
|-------|---------|
| idle > park age (PVC tiers) | sandbox suspended — resume serves the PVC directly, no S3 involved |
| idle > reap age | broker offloads `/workspace` → S3 (briefly resuming a suspended sandbox so a pod exists to snapshot), then deletes the pod + CR + per-session key — and for PVC tiers purges the chat dir from the hot tier |
| resolve after reap | broker creates a fresh pod, waits for readiness, then **synchronously** restores S3 → `/workspace` before the session becomes ready (only if the workspace is empty — hot-tier data always wins) |
| long-running session | a leader-gated task snapshots it every `broker.s3.periodicSyncInterval`, so a node loss loses at most one interval |
| offload failure on reap | retried with backoff; the pod + CR stay alive until success or `maxAttempts` (**no silent data loss**) |
| restore failure on resume | the resume **fails** (surfaced as an error); an empty workspace is never handed back |

## Provider + auth (bring-your-own)

Point the broker at any S3-compatible endpoint — AWS S3, MinIO, Cloudflare R2, Proxmox S3
API. Auth is a **static access key in a Kubernetes Secret** (portable), projected into the
broker at `/etc/s3-creds`:

```yaml
broker:
  persistentMode: empty-dir   # or per-user-pvc / shared-subpath for hybrid tiering
  s3:
    enabled: true
    endpoint: "https://s3.example.com"
    bucket: "open-websandbox-workspaces"
    prefix: "users"
    credentialsSecret: "owui-s3-creds"   # create this yourself (see below)
    retentionDays: 30
    compression: zstd
    periodicSyncInterval: 300
    sizeLimit: "2Gi"
    tmpfs: false
```

Create the credentials Secret yourself (the chart references it; it does **not** create it):

```bash
kubectl -n agent-sandbox-system create secret generic owui-s3-creds \
  --from-literal=access-key-id='AKIA...' \
  --from-literal=secret-access-key='...'
```

Transport is TLS (`https://` endpoint); objects are written with **SSE-S3** (`AES256`) at
rest by default (D9), controlled by `broker.s3.sse` (`AES256` | `aws:kms` | `""`). Set it to
`""` for S3-compatible stores without a KMS/SSE backend (e.g. dev MinIO, which rejects
SSE requests unless configured with a KMS). The broker reads the credentials from the
projected files (no S3 secret lands in pod env, consistent with #48).

## Retention (per-session lifetime)

Retention is **session-scoped**, not a global fixed window:

- The broker keeps **only the latest** object per session prefix — each offload deletes prior
  objects under `users/<uid-hash>/chats/<sid-hash>/`, so a prefix holds exactly one tarball.
- Each object is written with an `Expires` header (`now + retentionDays`) and
  `retention-days`/`session-snapshot` metadata (R2/D5).

### Recommended bucket lifecycle rule

As a portable age-out safety net (the broker intentionally does **not** auto-manage the
bucket lifecycle, to stay portable across AWS/MinIO/R2/Proxmox), apply a single lifecycle
rule on the bucket prefix that matches every session and expires objects after
`retentionDays`. AWS S3 / MinIO example:

```json
{
  "Rules": [{
    "ID": "open-websandbox-session-retention",
    "Status": "Enabled",
    "Filter": {"Prefix": "users/"},
    "Expiration": {"Days": 30}
  }]
}
```

MinIO / R2 expose the same `Expiration` via their console / `mc ilm` / API. A session that
is abandoned (never resumed) thus ages out of the cold tier even though the broker no longer
references it.

## Failure modes

- **Offload failure (reap)** — retried up to `BROKER_S3_OFFLOAD_MAX_ATTEMPTS` (default 5)
  with linear backoff; on exhaustion the Sandbox CR + per-session key are **kept alive** and
  retried on the next reaper tick. No snapshot is silently lost (D7).
- **Restore failure (resume)** — the resume returns an error (HTTP 502 from the broker);
  the pod is not handed to the user with an empty workspace (D7).
- **Workspace too large** — the runtime refuses a snapshot/restore that exceeds
  `MAX_WORKSPACE_BYTES` (= `broker.s3.sizeLimit`, default 2Gi) with HTTP 413 — **fail, never
  truncate** (D9). The `emptyDir` `sizeLimit` is the uncompressed backstop (kubelet
  eviction).

## Non-goals

- This mode does **not** change the vendored controller/CRDs.
- Per-object encryption-at-rest beyond SSE is out of scope; layer it at the bucket.
