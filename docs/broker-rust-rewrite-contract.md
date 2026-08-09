# open-websandbox broker — Rust rewrite contract (issue #18)

Source of truth: `open-websandbox-platform/broker/main.py` (1552 LOC, not 2100) + `requirements.in`.
Broker serves on **:8080** (uvicorn); runtime pods on **:8888**. All hops go **directly to `pod_ip:8888`** — `BROKER_ROUTER_URL` is defined but **never read** (dead config). `PERSISTENT_PREFIX`/`SHARED_PREFIX` env vars are likewise parsed but **never used** (dead config).

---

## 1. HTTP surface

**Auth model** (`_auth`, FastAPI `Security(HTTPBearer)`):

- Constant-time `hmac.compare_digest` against `BROKER_SHARED_SECRET`.
- Secret unset/placeholder → **503** "not configured" (fail-closed, defense-in-depth with the boot guard). Bad token → **401**.
- Placeholder denylist `_PLACEHOLDER_SECRETS = {"","dev-shared-secret-change-me","change-me","changeme","placeholder"}`.

**Broker-handled endpoints** (not proxied):

| Path | Method | Auth | Body | Response | Status |
|---|---|---|---|---|---|
| `/healthz` | GET | none | – | `{"status":"ok"}` | 200 |
| `/readyz` | GET | none | – | `{"status":"ready"}` | 200 / **503** (apiserver unreachable; live `list sandboxes limit=1 _request_timeout=3`) |
| `/metrics` | GET | none | – | Prometheus text (`prometheus_client.generate_latest`) | 200 |
| `/openapi.json` | GET | none | – | static `OPENAPI` dict (3.0.3, title `open-websandbox runtime API`, version `0.1.0`) | 200 |
| `/docs` | GET | none | – | inline Swagger-UI HTML (CDN assets) | 200 |
| `/api/config` | GET | Bearer | – | `{"features":{"terminal":true,"notebooks":false,"desktop":false}}` | 200 |
| `/api/status` | GET | Bearer | – | `{"active_pods":0,"max_pods":10,"pods":[]}` (static) | 200 |
| `/api/terminals/{session_id}` | **WS** | first-message `{"type":"auth","token":<secret>}` within 10 s | bidir text/bytes | relayed PTY | close **4001** (auth), **1008** (missing ids), **1011** (sandbox unavailable) |
| `/{path:path}` | GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS | Bearer + `X-User-Id` | raw streamed bytes | proxied runtime response | passthrough + 400/500/502/504 |

`include_in_schema=False` on all of the above → the auto-OpenAPI is suppressed (`openapi_url=None`, `docs_url=None`); the curated `OPENAPI` is the only schema exposure.

**Catch-all proxy** (`/{path:path}`):

- Requires `X-User-Id` header else **400**. `X-Session-Id` defaults to user id. `X-Persistence` header/query → profile (else `BROKER_DEFAULT_PROFILE`).
- `resolve_sandbox(user, session, profile)` → `(sandbox_id, pod_ip)`.
- Strips hop-by-hop headers (`HOP` = connection, keep-alive, proxy-*, te, trailers, transfer-encoding, upgrade, host, content-length, authorization).
- Injects `X-Sandbox-Id`, `X-Sandbox-Namespace`, `X-Sandbox-Pod-IP`, `X-Session-Id`, and `Authorization: Bearer <per-session key>`.
- Streams request body + response (`httpx stream=True`). Rewrites redirect `Location` (301/302/303/307/308) to drop host so clients re-enter the broker.
- OTel span `broker.runtime_hop` with attributes `sandbox.id`, `sandbox.pod_ip`, `http.method`, `http.route`, `http.status_code`.
- Broker-raised statuses: **400** (no user id), **500** (sandbox create failed), **504** (not ready in `CLAIM_READY_TIMEOUT`), **502** (S3 restore failed).

**WS terminal relay**: identity from query `user_id`/`session_id`/`chat_id` or headers `x-user-id`/`x-session-id`, fallback to path `{session_id}`. Pre-attaches a PTY via idempotent `POST pod_ip:8888/api/terminals` with `X-Session-Id`. Plaintext `ws://pod_ip:8888/api/terminals/{session_id}` (TLS terminates at ingress). Two pump tasks (`FIRST_COMPLETED`), cancel both, close upstream first so the runtime's PTY cleanup runs (anti-leak vs per-pod cap → 429).

**Curated OpenAPI** (`openapi_spec.py`, served at `/openapi.json`) — this is the **runtime's** LLM-facing tool surface that Open WebUI discovers (broker serves it statically; the proxy forwards the actual calls): `/execute`, `/files/{cwd,list,read,write,mkdir,move,delete,replace,grep,glob,upload,archive,view}`, `/upload`, `/download|/list|/exists /{file_path}`, `/api/terminals[/{id}]`, `/api/config`, `/api/status`, `/healthz`.

---

## 2. k8s API interactions

**API clients**: `kubernetes` python client, `load_incluster_config()` (fallback `load_kube_config()`). Two apis: `CustomObjectsApi()` (CRDs) + `CoreV1Api()` (Secrets/PVCs) + lazy `CoordinationV1Api()` (Leases). **Fully untyped** — all CRD bodies are raw dicts.

**Resources / verbs** (namespace = `RUNTIME_NS` = `agent-sandbox-runtime` unless overridden):

| GVK | Plural | Verbs used |
|---|---|---|
| `agents.x-k8s.io/v1beta1` **Sandbox** | `sandboxes` | get, list, create, patch, delete, **watch** |
| `extensions.agents.x-k8s.io/v1beta1` **SandboxTemplate** | `sandboxtemplates` | get (read `BASE_TEMPLATE` podTemplate, clone+mutate) |
| `core/v1` **Secret** | secrets | create, patch, read, list (label-selector), delete |
| `core/v1` **PersistentVolumeClaim** | pvcs | read, create (per-user-pvc mode only) |
| `coordination.k8s.io/v1` **Lease** | leases | create, read, replace |

Note: `SandboxClaim`/`SandboxWarmPool` CRDs are **vendored but NOT used by the broker** — the warm-pool path was removed (cannot carry a per-session Secret). The broker creates direct `Sandbox` objects for both profiles.

**Leader election** — **hand-rolled**, NOT the official `kubernetes` leader-election helper. Single `Lease` (`owui-broker-leader`, ns=`BROKER_LEADER_NAMESPACE`):

- read lease; if absent → create with `holderIdentity`/`acquireTime`/`renewTime`/`leaseDurationSeconds`.
- if held by other AND `renewTime` within duration → defer (False).
- else (ours / other-expired) → `replace` lease with our identity + fresh times (takeover).
- renew loop every `BROKER_LEADER_RENEW_SECONDS` (5 s), duration 15 s. **Only the leader runs the reaper + S3 periodic-sync; the request path runs on every replica.**

**Watchers**: **no long-lived informers**. `_watch_until_ready()` is an ad-hoc single-object watch: GET → if ready return; else `Watch().stream(list_namespaced_custom_object, field_selector=metadata.name=<name>, resource_version=<rv>, timeout_seconds=<remaining>)`, invoking `on_event(name,obj)` per event until `is_ready` (Ready condition True **and** podIP present) or deadline. Run via `asyncio.to_thread`. `on_event=_resume_if_suspended` flips Suspended→Running.

**Reaper loop** (leader-gated, `while True`, `BROKER_REAPER_POLL_SECONDS` default 60 s):

- `list sandboxes` with `label_selector=app.kubernetes.io/managed-by=owui-broker`.
- idle = `now - int(annotation "broker-last-used")`. Per sandbox:
  - **persistent + s3-tiered**: idle > `IDLE_TTL` → offload `/workspace→S3` (retry+backoff) → on success delete sandbox+key; on failure **keep pod+CR alive** (no silent loss), retry next tick.
  - **persistent (pvc/subpath)**: idle > `REAP_TTL` (7 d) → delete; elif idle > `PARK_TTL` (120 s) → patch `spec.operatingMode=Suspended` (park).
  - **ephemeral**: idle > `IDLE_TTL` (120 s) → delete.
- recomputes `ACTIVE_SANDBOXES` gauge by profile; runs `_sweep_orphan_runtime_keys`.

**Periodic S3 sync** (R1, leader-gated, every `S3_PERIODIC_SYNC_SECONDS` default 300 s): offload every Running s3-tiered sandbox (best-effort, `final=False`, no keep-alive).

---

## 3. Per-session key lifecycle (#50)

Broker is **stateless** — the per-session runtime key is the **single source of truth in a k8s Secret**, read on every hop (HA-safe across replicas, no DB/in-memory cache).

- Secret name: `owui-runtime-key-<sandbox>` (`BROKER_RUNTIME_KEY_PREFIX`), ns `RUNTIME_NS`, labels `managed-by=owui-broker` + `owui.io/component=runtime-key`, `stringData.api-key`.
- `_mint_runtime_key()` = `secrets.token_urlsafe(32)` (256-bit URL-safe).
- **Create**: `_ensure_runtime_key` — get-or-create, minted **before** Sandbox create so the (non-optional) projected Secret volume is satisfiable at pod creation.
- **Inject**: `_inject_runtime_key_volume` adds volume `runtime-key` (secret, items `api-key→api-key`) + readOnly mount `/etc/runtime-key` on every container.
- **Rotate-on-resume**: `_rotate_runtime_key` mints a **fresh** key **before** a parked (Suspended) sandbox resumes — a key observed by the prior pod can't be replayed against the resumed pod. Branch in `resolve_sandbox`: `just_created`→ensure; `elif Suspended`→rotate.
- **Per-hop auth**: `_runtime_auth_headers` → stateless `read_namespaced_secret`, base64-decode `data.api-key`; returns `{}` on 404 (runtime fails closed 401/503).
- **Reap**: `_delete_runtime_key` with the sandbox (best-effort, 404-tolerant).
- **Orphan sweep (#51)**: `_sweep_orphan_runtime_keys(live_names)` — list Secrets `label_selector=managed-by=owui-broker`, derive owner from `owui-runtime-key-<sandbox>` prefix, delete any whose Sandbox is gone. Catches a crash window between delete-Sandbox and delete-key (or mint-key + failed create). Runs each reaper tick, idempotent.

---

## 4. S3-tiered mode (#52)

Broker is the **sole S3 client** (runtime pod stays network-isolated per #50). Hot tier = size-limited **emptyDir** `/workspace` (default 2 Gi, optional tmpfs); cold tier = bring-your-own S3-compatible bucket. Object key: `<S3_PREFIX>/<sha256(uid)[:16]>/chats/<sha256(sid)[:16]>/workspace-<ts:010d>.tar.zst` (lexical==chronological, content-hashed to avoid PII in keys).

- **Offload-on-reap** (`_offload_to_s3`): `GET pod:8888/snapshot` (stream, per-session Bearer) → multipart PutObject (SSE-S3 `AES256` when `S3_SSE`, `ContentType=application/zstd`, `Expires=now+retention`, `Metadata={session-snapshot,retention-days}`) → **then** delete prior objects under prefix **except the just-uploaded key** (keep-latest; prefix never empty mid-offload → a crash leaves the previous snapshot restorable, D7/#56). Counter `s3_offload_total{kind=final|periodic}`.
- **Restore-on-resume** (`_restore_from_s3`): `list_objects_v2(prefix)` → `max(keys)` (lexical newest) → `get_object` stream → `PUT pod:8888/restore` (streaming body). **No-op** on empty prefix (first creation). **Raises on restore failure → 502** (never serve an empty workspace, D7). Counter `s3_restore_total`.
- **Retry/backoff** (`_offload_to_s3_with_retry`): up to `S3_OFFLOAD_MAX_ATTEMPTS` (5), backoff `BACKOFF*attempt`; returns False on exhaustion → caller keeps pod+CR alive (no silent data loss).
- **Periodic sync (R1)**: leader-only, every `S3_PERIODIC_SYNC_SECONDS` (300 s), snapshots all Running s3-tiered sandboxes (`final=False`, best-effort, no keep-alive). The on-reap offload remains authoritative.
- **keep-latest retention**: one object per session prefix; `_s3_delete_prefix(skip=latest)`.
- **MinIO-compat delete quirk**: `_s3_delete_prefix` uses **per-object `delete_object`**, NOT batch `DeleteObjects` — MinIO/some S3-compatible stores require a `Content-MD5` header on batch DeleteObjects that botocore doesn't emit. **Must replicate in Rust** (per-key `delete_object`, not `delete_objects`).
- Path-style addressing (`addressing_style=path`) for MinIO/R2/Proxmox/AWS. Creds read from **files** under `S3_CREDS_DIR` (`access-key-id`, `secret-access-key`), not env. `_get_s3_client` is the **test seam** (monkeypatched).
- Boot guard: `s3-tiered` mode without `S3_ENABLED`, or `S3_ENABLED` without `S3_ENDPOINT`+`S3_BUCKET` → `RuntimeError` (fail-closed, no silent data loss).

---

## 5. Persistent modes (3-way branch in `_create_sandbox` / `_persistent_volume`)

`BROKER_PERSISTENT_MODE` ∈ {`per-user-pvc` (default), `shared-subpath`, `s3-tiered`}.

- **per-user-pvc**: one dedicated PVC `workspace-p-<sha256(uid)[:12]>` per user (cephfs RWX, `storageClassName=BROKER_PERSISTENT_STORAGECLASS`, size `BROKER_PERSISTENT_STORAGE`), get-or-create (`_ensure_user_pvc`). Each chat mounts the whole PVC at `/workspace` via `subPath=<sha256(sid)[:16]>/` (chat-isolated top-level folder). PVC persists across all the user's chats/sessions.
- **shared-subpath**: one static shared PVC (`BROKER_SHARED_PVC`, default `workspace-shared`); each chat mounts `subPath=users/<uid>/<sha256(sid)[:16]>/` (hard isolation between users and chats).
- **s3-tiered**: no PVC; `/workspace` = the template's emptyDir **forced** to `sizeLimit=BROKER_S3_SIZE_LIMIT` (+ optional `medium=Memory` tmpfs). Cold tier = S3 (broker-orchestrated offload/restore).

The podTemplate is **cloned from `BASE_TEMPLATE` SandboxTemplate** then mutated (workspace volume replaced; runtime-key volume injected; `shutdownPolicy=Retain` so park keeps identity+key). Ephemeral profile leaves the template's emptyDir workspace untouched.

Staging migration (`_migrate_staging_to_chat`): when a persistent chat is created and `session_id != user_id`, the user's "staging" sandbox (`_chat_sandbox_name(user, user)`) is reached, its `/workspace` archived→uploaded→`python3`-extracted into the chat, then staging wiped (anti cross-chat leak). Hops use the **target** sandbox's per-session key. Unreachable staging → deleted (disposable).

---

## 6. Metrics / OTel (#49)

**Prometheus** (`prometheus_client`, exposed at `/metrics`, `include_in_schema=False`):

- `open_websandbox_broker_http_requests_total{method,path,status}` Counter
- `open_websandbox_broker_http_request_duration_seconds{method,path,status}` Histogram
- `open_websandbox_broker_active_sandboxes{profile}` Gauge (leader view)
- `open_websandbox_broker_sandboxes_created_total{profile}` Counter
- `open_websandbox_broker_sandboxes_deleted_total{profile}` Counter
- `open_websandbox_broker_runtime_hop_errors_total` Counter
- `open_websandbox_broker_s3_offload_total{kind}` Counter (kind=final|periodic)
- `open_websandbox_broker_s3_restore_total` Counter

**Path-label collapsing** (`_route_template`): the middleware reads the **matched Route's templated path** from `request.scope["route"].path` (e.g. `/api/terminals/{session_id}`, `/healthz`, `/{path:path}`) **after** routing resolves it — never the raw URL, so the catch-all proxy can't mint a unique label per request (cardinality bomb). `unmatched` fallback.

**Soft OTel**: `_setup_telemetry` is a **complete no-op** unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set AND `opentelemetry-*` importable (soft-import, not in requirements.in). When active: `FastAPIInstrumentor.instrument_app(excluded_urls="healthz,readyz,metrics")` + `HTTPXClientInstrumentor` on the shared client → end-to-end broker→runtime trace context. `_NoOpTracer`/`_NoOpSpan` absorb all calls when inactive. Manual span `broker.runtime_hop`. Resource: `service.name=OTEL_SERVICE_NAME|open-websandbox-broker`, `service.namespace=open-websandbox`.

---

## 7. Config / env vars

**Required / fail-closed** (validated in `_validate_config` at startup + `_auth` per request):

- `BROKER_SHARED_SECRET` — must NOT be in `_PLACEHOLDER_SECRETS`, else `RuntimeError`.
- (if `BROKER_S3_ENABLED`) `BROKER_S3_ENDPOINT` + `BROKER_S3_BUCKET`.
- (if `BROKER_PERSISTENT_MODE=s3-tiered`) `BROKER_S3_ENABLED` must be true.

**Optional — identity / naming** (with defaults):

- `BROKER_RUNTIME_NS`=`agent-sandbox-runtime`
- `BROKER_BASE_TEMPLATE`=`code-standard-v1`
- `BROKER_RUNTIME_KEY_PREFIX`=`owui-runtime-key-`
- `BROKER_CLAIM_PREFIX`=`owui-` (ephemeral sandbox names)
- `BROKER_CHAT_PREFIX`=`owui-c-`
- `BROKER_PER_USER_PVC_PREFIX`=`workspace-p-`
- `BROKER_ROUTER_URL`=`http://sandbox-router-svc.agent-sandbox-system:8080` — **DEAD (parsed, never read)**
- `BROKER_PERSISTENT_PREFIX`=`owui-p-`, `BROKER_SHARED_PREFIX`=`owui-s-` — **DEAD (parsed, never read)**

**Optional — profile / mode**:

- `BROKER_DEFAULT_PROFILE`=`persistent` ∈ {ephemeral,persistent} (deploy-fixed; OWUI can't send headers; valid `X-Persistence` overrides for admin/test)
- `BROKER_PERSISTENT_MODE`=`per-user-pvc` ∈ {per-user-pvc, shared-subpath, s3-tiered}

**Optional — PVC backing**:

- `BROKER_PERSISTENT_STORAGECLASS`=`cephfs`
- `BROKER_PERSISTENT_STORAGE`=`10Gi`
- `BROKER_SHARED_PVC`=`workspace-shared`

**Optional — TTLs / timeouts** (`_env_int`):

- `BROKER_IDLE_TTL_SECONDS`=120 (ephemeral + s3-tiered reap)
- `BROKER_PARK_IDLE_SECONDS`=120 (persistent park→Suspended)
- `BROKER_REAP_SECONDS`=604800 (7 d, persistent reap)
- `BROKER_REAPER_POLL_SECONDS`=60
- `BROKER_CLAIM_TIMEOUT_SECONDS`=60 (readiness wait)
- `BROKER_PROXY_TIMEOUT_SECONDS`=660 (httpx client + proxy)

**Optional — leader election**:

- `BROKER_LEADER_NAMESPACE`=`RUNTIME_NS`
- `BROKER_LEADER_LEASE`=`owui-broker-leader`
- `BROKER_LEADER_DURATION_SECONDS`=15
- `BROKER_LEADER_RENEW_SECONDS`=5
- `HOSTNAME` (k8s sets; fallback `broker-<pid>`)

**Optional — S3 cold tier**: `BROKER_S3_ENABLED`="", `BROKER_S3_ENDPOINT`="", `BROKER_S3_BUCKET`="", `BROKER_S3_PREFIX`=`users`, `BROKER_S3_REGION`=`us-east-1`, `BROKER_S3_CREDS_DIR`=`/etc/s3-creds`, `BROKER_S3_RETENTION_DAYS`=30, `BROKER_S3_PERIODIC_SYNC_SECONDS`=300, `BROKER_S3_SIZE_LIMIT`=`2Gi`, `BROKER_S3_TMPFS`="", `BROKER_S3_SSE`=`AES256` ("" disables), `BROKER_S3_OFFLOAD_MAX_ATTEMPTS`=5, `BROKER_S3_OFFLOAD_BACKOFF_SECONDS`=10, `BROKER_S3_PART_SIZE_BYTES`=8388608.

**Optional — OTel (soft)**: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME`.

---

## 8. Direct deps (requirements.in) → Rust crates

| Python dep | Purpose | Rust candidate |
|---|---|---|
| `fastapi==0.141.1` | HTTP framework + routing + middleware | **axum** |
| `uvicorn[standard]==0.52.1` | ASGI server (incl. uvloop, httptools, websockets, watchfiles, dotenv) | **hyper** + **tokio** (rt-multi-thread) |
| `httpx==0.28.1` | async HTTP client (proxy + runtime hops), shared pool | **reqwest** (streaming) |
| `kubernetes==36.0.3` | k8s API (CRDs untyped, Watch, Leases) | **kube-rs** (`kube` + `DynamicObject`/`Api`) |
| `websockets==17.0.1` | WS client relay (terminal PTY, text+bytes) | **tokio-tungstenite** |
| `prometheus_client==0.26.0` | counters/gauges/histograms + exposition | **prometheus** crate (or **metrics** + **metrics-exporter-prometheus**) |
| `aioboto3==15.5.0` | async S3 (multipart, list, get/put, delete) — **soft-imported** | **aws-sdk-s3** (aws-sdk-rust), feature-gated |

OTel is **not** in requirements.in (soft-imported opentelemetry-*); Rust equivalent **opentelemetry** + **opentelemetry-otlp** + **tracing-opentelemetry**, also feature-gated. `botocore` rides along with aioboto3 (config: path-style, retries) — subsumed by aws-sdk-s3 builder config.

---

## 9. Hard-to-port flags (dynamic / Python-specific)

1. **Soft-imports gating the boot path**: `aioboto3`/`botocore` only when `S3_ENABLED`; `opentelemetry-*` only when `OTEL_EXPORTER_OTLP_ENDPOINT`. The broker must **boot + serve with neither installed**. → Rust: `#[cfg(feature="s3")]` / `#[cfg(feature="otel")]` optional deps + runtime no-op tracers.
2. **Untyped/dynamic k8s CRD client**: all Sandbox/SandboxTemplate bodies are raw dicts via `CustomObjectsApi` — no generated types. → `kube::Api::<DynamicObject>` with explicit `GroupVersionKind` per resource; must hand-build the same patch/merge JSON-strategy bodies.
3. **Hand-rolled leader election** (not the SDK helper): exact read/create/replace Lease semantics (`holderIdentity`, `acquireTime`, `renewTime`, `leaseDurationSeconds`, **takeover-when-other-expired**, defer-when-other-live) must be reproduced field-for-field; the official Rust leader-election crate exists but behavior must be audited to match.
4. **Ad-hoc single-object Watch** (`Watch().stream` with `resource_version` + `field_selector=metadata.name=` + `timeout_seconds` + `on_event` callback + deadline) is not a standard kube-rs informer pattern — needs a bespoke list-then-watch-until-predicate loop.
5. **Streaming bidirectional proxy** (httpx `stream=True` both ways) + hop-by-hop header stripping + **redirect Location rewrite** + injected headers; the **WS relay is a separate code path** (text+bytes pumps, `FIRST_COMPLETED` cancel, close-upstream-first for PTY cleanup). Two distinct transport implementations in Rust (hyper for HTTP, tungstenite for WS).
6. **aioboto3 async streaming multipart** (`resp.aiter_bytes()` directly into `upload_part`) + the **MinIO per-object-delete quirk** (no batch DeleteObjects) + **path-style addressing** + **file-based creds** + **conditional SSE-S3** must all be reproduced on `aws-sdk-s3` (multipart manager + `force_path_style(true)` + conditional `server_side_encryption`).
7. **Annotation-driven TTL reaper** (idle = `now - annotation "broker-last-used"`, patched on every hop) with per-profile/per-mode branching + retry+backoff keep-alive — logic-heavy, no k8s TTL to lean on.
8. **`_PLACEHOLDER_SECRETS` fail-closed** list must be replicated exactly (boot guard + per-request guard); easy to accidentally make a "disabled" mode.
9. **Route-template cardinality collapsing**: axum doesn't expose the matched route template the same way Starlette does (`scope["route"].path`) — must wire a middleware that maps the matched path to a low-cardinality template (else the catch-all proxy bombs the label set).
10. **Staging migration depends on `python3` inside the runtime image** (`zipfile.extractall` via `POST /execute`) — a Rust rewriter of the broker doesn't touch this, but it couples broker↔runtime image contents; flag for the contract.
11. **`_get_s3_client` / `_coord` test seams** are monkeypatched in tests — a Rust port needs an equivalent trait-based seam for the offload/restore/leader paths or the e2e/unit tests can't inject fakes.

---

## Top 5 porting risks

The single largest risk is the **hand-rolled k8s control plane** (leader election + ad-hoc single-object watches + untyped CRD mutation + annotation-TTL reaper) — these are subtle, behavior-critical, and not 1:1 with any Rust SDK helper, so a faithful reimplementation must be re-validated against the live agent-sandbox controller, not just unit-tested. Second, the **streaming bidirectional proxy + WS relay** is two non-trivial transports (HTTP hop-by-hop/redirect-rewrite/injection, and a PTY WS pump with careful close ordering to avoid per-pod PTY leaks) that must be byte-faithful or OWUI terminals/files break silently. Third, **S3-tiered data safety** (offload-then-delete ordering, keep-latest, MinIO per-object-delete, retry+keep-alive-on-failure, restore-fails-closed) is a data-loss surface where a naive Rust port will silently drop workspaces on reap. Fourth, **soft-import/feature-gating + the fail-closed secret model** must be preserved exactly or the broker either fails to boot in the default S3-off deployment or, worse, runs with auth disabled. Fifth, **cardinality-safe metrics** (matched-route-template labels, not raw URLs) and the **stateless per-session-key Secret model** (#50) are invariants that are easy to violate in a rewrite and only manifest as Prometheus OOM or cross-session auth regressions in production.
