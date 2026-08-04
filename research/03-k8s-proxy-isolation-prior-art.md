# open-terminal-k8s-proxy — Isolation Design (Prior Art / Requirements Baseline)

Source repo: `open-terminal-k8s-proxy` (commit at working tree, 2026-07-26).
This documents the isolation model the existing Kubernetes-based proxy implements (and, where noted,
only *designs*), so the Cloudflare-"computer" replacement can reproduce or improve on it.

> **How to read this doc.** Where a feature exists in code, citations are `file:line` from the
> `terminal_proxy/` package. Where a feature is only an OpenSpec *proposal/design* (not yet in code),
> the citation is to `openspec/changes/.../...md` and is explicitly flagged as **DESIGNED, NOT IMPLEMENTED**.
> The two OpenSpec changes that are *fully* shipped in code are `per-chat-isolation` and the core
> per-user fan-out; `peruserperchat-stability` and `user-based-network-policy` are partially / not
> implemented (their `tasks.md` checkboxes are unchecked).

---

## 1. What the proxy does

`open-terminal-k8s-proxy` is a FastAPI reverse proxy + Kubernetes orchestrator that sits between
**Open WebUI** and one-or-many `open-terminal` pods. A single `open-terminal` container is single-user
and stateful to one process tree; it cannot, by itself, serve multiple OWUI users safely (shared FS,
shared shell, shared API key, no per-user lifecycle). The proxy solves that by treating each request's
`X-User-Id` header as a tenant key, **dynamically provisioning a dedicated terminal pod per user**
(`terminal-<hash>`) on demand, and transparently reverse-proxying every terminal/file/execute/desktop
call to that user's pod. It owns the full per-user resource lifecycle in Kubernetes (Secret for the
pod's API key, Service, optional PVC, and — by design — per-user NetworkPolicy), enforces idle/pod
caps, and on top of that layers **per-chat working-directory isolation** (and optionally
**pod-per-chat**) keyed by Open WebUI's `X-Session-Id`. README §Overview (`README.md:3-12`):
*"Accepts requests from Open WebUI with a `X-User-Id` header; Creates a dedicated terminal pod for
each user (on demand); Proxies all requests to the user's pod; Manages pod lifecycle."*

---

## 2. Architecture & request flow

```
Open WebUI ──HTTP/WS──► K8s Proxy (FastAPI, :8000) ──► per-user terminal pod (ClusterIP svc)
                              │  uses CoreV1Api: creates Pod + Secret + Service (+ optional PVC)
                              └─ routes by (X-User-Id [, X-Session-Id]) header → pod lookup/create
```

**End-to-end trace** (HTTP, e.g. `POST /files/write`):

1. Open WebUI calls the proxy with headers `Authorization: Bearer <PROXY_API_KEY>`, `X-User-Id`, and
   (for chats) `X-Session-Id`. `verify_api_key` checks the bearer token (`main.py:95-103`).
2. The route handler runs `extract_user_id(request)` which **requires** the `X-User-Id` header, else
   `400` (`main.py:106-111`); `extract_chat_id(request)` reads the optional `X-Session-Id`
   (`main.py:114-116`).
3. Handler calls `get_terminal_for_user(user_id, chat_id)` (`main.py:130-142`). Routing decision:
   - **`perChat` / `perUserPerChat` modes** → `pod_manager.lookup(user_id, chat_id)` is **lookup-only**
     (returns the running pod for the `(user, chat)` composite key, or `None` → `503`). Reads/polls
     never create pods (`pod_manager.py:173-183`, `main.py:137-141`).
   - **`perUser` mode** (default) → `_create_terminal_for_user()` get-or-creates the one shared pod
     (`main.py:142`, `_create_terminal_for_user` `main.py:145-172`).
4. Only `POST /api/terminals` (terminal *creation*) goes through the create path
   (`_create_terminal_for_user(..., force_chat_dir=True)`, `main.py:728-729`).
5. Routing key: `PodManager._pod_key(user_id, chat_id)` = `user_hash` in `perUser`, or
   `f"{user_hash}-{chat_hash}"` in per-chat modes (`pod_manager.py:165-171`).
6. The resolved pod's HTTP endpoint is `http://terminal-<key>:8000` (`models.py:87-90`, `endpoint`).
   `http_proxy.proxy_request(...)` re-issues the request, rewriting `Authorization` to the pod's own
   key and stripping hop-by-hop headers (`proxy/http.py:50-132`).
7. WebSocket path (`/api/terminals/{session_id}`, `/desktop/vnc`): authenticates the first frame with
   `PROXY_API_KEY`, then reads `user_id` from the **query string** and `X-Session-Id` from headers
   (`main.py:760-803`, `main.py:1040-1077`); bridges client↔upstream via aiohttp
   (`proxy/websocket.py:36-101`).

**How pods are created — raw Pod, not Job/Deployment.** `PodManager.get_or_create`
(`pod_manager.py:195-255`) builds a `TerminalPod` model (`models.py:97-131`) and calls
`_create_pod_resources` (`pod_manager.py:257-328`), which:

- ensures the shared/per-user PVC (`storage.py`),
- builds manifests via `build_pod_for_user` (`k8s/pod_builder.py:285-346`),
- `create_or_get_secret` (idempotent, adopts leftover + its key, `k8s/client.py:250-265`),
- **creates the Pod** with `restartPolicy: Never` via `k8s_client.create_pod(pod_manifest)`
  (`k8s/client.py:103-105`, `pod_manager.py:295`; manifest `restartPolicy: "Never"` at
  `k8s/pod_builder.py:199`),
- `create_or_get_service` (`k8s/client.py:198-209`, `pod_manager.py:299`),
- `wait_for_pod_ready` (polls phase + `GET /health` on the pod, `k8s/client.py:281-322`).

Quoting the actual create call:

```python
# terminal_proxy/pod_manager.py:295
k8s_client.create_pod(pod_manifest)
# terminal_proxy/k8s/client.py:103-105
def create_pod(self, pod_manifest: dict[str, Any]) -> V1Pod:
    """Create a pod from the given manifest."""
    return self.core_v1.create_namespaced_pod(self.namespace, pod_manifest)
```

There is **no** Kubernetes Job, Deployment, or ReplicaSet — pods are bare `kind: Pod`
(`restartPolicy: Never`); the proxy reconciles them itself on restart
(`pod_manager.py:58-100`, `_reconcile_existing_pods`).

---

## 3. User isolation model

**Identity propagation.** Open WebUI sends the user identity in a single header,
`X-User-Id`, which the proxy **requires**:

```python
# terminal_proxy/main.py:106-111
def extract_user_id(request: Request) -> str:
    """Extract user ID from request headers."""
    user_id = request.headers.get("X-User-Id")
    if not user_id:
        raise HTTPException(status_code=400, detail="X-User-Id header required")
    return user_id
```

There is **no email/UPN join, no OIDC, no DB lookup** in this repo. The task brief mentioned an
"email-based identity mapping" design — **it does not exist here.** `X-User-Id` is whatever string
Open WebUI puts in that header (in practice the OWUI user id), and it is mapped to a stable
12-char hash by `user_id_to_hash` (`models.py:15-17`): `sha256(user_id)[:12]`. That hash becomes the
Kubernetes object-name suffix, the pod label `user-id-hash`, and the storage `subPath`.
(See §7 below for the explicit "no email mapping" finding.)

**What "user isolation" concretely means** — each boundary and its enforcement:

| Boundary | Mechanism | Enforcement (file:line) |
|---|---|---|
| **Separate pod per user** | `PodManager` keys `_pods` by `user_hash`; one pod name `terminal-<user_hash>` | `pod_manager.py:165-171`, `models.py:120-131` |
| **Separate API key per user/pod** | Random `secrets.token_urlsafe(32)`; stored in a per-pod `Secret`, injected as `OPEN_TERMINAL_API_KEY` | `pod_manager.py:140-141`, `k8s/pod_builder.py:148-161`, `main.py:95-103` (proxy rejects callers without `PROXY_API_KEY`) |
| **Separate Service (network identity)** | `ClusterIP` Service `terminal-<user_hash>` selecting `user-id-hash` label; proxy talks only to the user's own svc | `k8s/pod_builder.py:243-282`, `models.py:87-90` |
| **Separate filesystem (optional)** | `perUser` storage mode → dedicated `PVC pvc-<user_hash>` (RWO) mounted at `/data`; `shared`/`sharedRWO` → shared PVC with `subPath: <user_hash>` | `storage.py:80-110`, `k8s/pod_builder.py:104-130` (`subPath` at `:128-129`), `config.py:84-107` |
| **OS-level user identity** | **Not** used for isolation between users — all terminal pods run as the **same** uid/gid `1000` (`fsGroup: 1000`, `runAsUser: 1000`) | `k8s/pod_builder.py:212-216`, `values.yaml:120-138` |
| **Separate namespace per user** | **No** — all pods live in one configurable namespace (default `default`, `config.py:40-43`) | `config.py:40-43` |
| **Per-user egress policy** | **DESIGNED, NOT IMPLEMENTED** — see §6 | `openspec/changes/user-based-network-policy/` |

Crucially, cross-user isolation relies on **separate pods + separate Services + separate PVCs/subPaths**,
*not* on separate OS users or namespaces. A user's pod only ever mounts that user's PVC (or that
user's `subPath` of the shared volume), so one user cannot see another's files; the pod-to-pod network
is also isolated by the static Helm `NetworkPolicy` (only the proxy may reach pods on :8000,
`templates/networkpolicy-terminal.yaml:23-30`).

---

## 4. Chat isolation model

**Granularity of a "chat".** A chat = one `X-Session-Id` header value, supplied by Open WebUI
per-conversation. Extracted at `main.py:114-116`. Two coexisting mechanisms, depending on `podMode`:

1. **`perUser` mode (default): folder-per-chat.** One pod is shared by all of a user's chats, but
   each chat gets its **own working directory** `<data_mount_path>/<sanitized-chatid>` created and
   seeded as the session cwd **before** the PTY is spawned. Done entirely proxy-side against the pod's
   own `/files` API (no upstream change). `terminal_proxy/chat_bootstrap.py:23-74`:

   ```python
   # chat_bootstrap.py:46-72  (ensure_chat_dir, perUser path)
   slug = sanitize_chat_id(chat_id)
   chat_dir = f"{cfg.data_mount_path.rstrip('/')}/{slug}"
   ...
   resp = await client.post(f"{base}/files/mkdir", headers=auth, json={"path": chat_dir})
   ...
   headers = {**auth, "X-Session-Id": chat_id}
   resp = await client.post(f"{base}/files/cwd", headers=headers, json={"path": chat_dir})
   ...
   terminal.bootstrapped_chats.add(chat_id)   # cached per (pod, chat)
   ```

   The `X-Session-Id` header is forwarded transparently (only `host/content-length/transfer-encoding/
   connection/authorization` are stripped, `proxy/http.py:16-19`), so open-terminal binds the session
   to that cwd in its in-memory map. Triggered in `main.py:163` (`ensure_chat_dir(...)`) on every
   terminal-resolving call; `POST /api/terminals` forces a re-mkdir in case the dir was deleted
   mid-session (`main.py:729`, `main.py:157-164`).

2. **`perChat` / `perUserPerChat` modes: pod-per-chat + `--cwd` pinning.** Each *(user, chat)* pair
   gets its **own pod**, launched directly with `open-terminal run --cwd <mount>/<sanitized-chatid>`
   so the pod's *entire* working directory (terminal + file browser) is that chat's dir. From
   `k8s/pod_builder.py:137-187`:

   ```python
   # k8s/pod_builder.py:137-187
   cwd_target: str | None = None
   if has_volume:
       base_dir = cfg.data_mount_path
       if terminal_pod.chat_id and terminal_pod.chat_hash:
           cwd_target = f"{base_dir}/{sanitize_chat_id(terminal_pod.chat_id)}"
       else:
           cwd_target = base_dir
   ...
   if cwd_target:
       env.append({"name": "HOME", "value": cwd_target})
   ...
   if cwd_target:
       container["args"] = ["run", "--cwd", cwd_target]
   ```

   Because `os.chdir` fails on a missing dir, a pod-per-chat includes an **initContainer** that
   `mkdir -p`s the chat dir on the RWX volume before the main container starts
   (`k8s/pod_builder.py:201-210`):

   ```python
   # k8s/pod_builder.py:201-210
   if terminal_pod.chat_hash and cwd_target:
       spec["initContainers"] = [{
           "name": "init-chat-dir",
           "image": cfg.terminal_image,
           "command": ["sh", "-c", f"mkdir -p {shlex.quote(cwd_target)}"],
           "volumeMounts": volume_mounts,
       }]
   ```

   Quoting the design rationale (`openspec/changes/per-chat-isolation/design.md:91-103`):
   > "In [`perChat`] each chat pod ... is launched ... `--cwd <mount>/<sanitized-chatid>`, so
   > `open-terminal` starts already scoped to that chat's directory — `fs.home` *is* chat dir
   > (isolating file browser too) — no runtime mkdir/cwd bootstrap needed. ... every chat pod mounts
   > the **same** shared RWX volume, per-chat folders live side-by-side on it. `os.chdir` fails if
   > target does not exist, so per-chat pod includes an **initContainer** ... runs
   > `mkdir -p <mount>/<sanitized-chatid>` ... before main container starts."

   Spec scenarios: per-chat-workdirs `openspec/changes/per-chat-isolation/specs/per-chat-workdirs/spec.md:29-61`
   ("two terminal sessions created [with] different `X-Session-Id` values [for the] same user ...
   [each] SHALL resolve relative paths [in its] own `<dataMountPath>/<sanitized-chatid>` directory
   [and] SHALL NOT share working directory"); pod-per-chat-mode
   `openspec/changes/per-chat-isolation/specs/pod-per-chat-mode/spec.md:45-59` ("each chat pod SHALL
   be launched with `--cwd <dataMountPath>/<sanitized-chatid>`").

**Same-user, cross-chat: shared vs separated.**

| Aspect | `perUser` (folder-per-chat) | `perChat` / `perUserPerChat` |
|---|---|---|
| Pod / process tree | **Shared** (one pod/user) | **Separated** (one pod/chat) |
| Filesystem location | Shared PVC, **separated** by per-chat subdir | Shared RWX PVC, **separated** by per-chat subdir |
| Working directory | **Separated** (cwd pinned per session) | **Separated** (pod-level `--cwd`) |
| Shell environment / env vars | Shared (same process serves all chats sequentially) | Separated (fresh process per chat) |
| Background processes (`/execute`) | Shared within the user's pod | Separated per chat pod |

So in `perUser` mode chats of the same user are *filesystem-isolated* but *process-isolation is
weak* (one pod, one open-terminal instance, sessions multiplexed by `X-Session-Id`). `perChat` /
`perUserPerChat` strengthen this to full process + environment isolation per chat, at the cost of
more pods. `--cwd` is pinned in **both** modes (the fix for the original "terminal opens in `/`"
bug): `perUser` pins `--cwd <mount>` then scopes per-chat via runtime cwd; `perChat` pins
`--cwd <mount>/<chatid>` directly.

**Chat-id sanitization.** Because `X-Session-Id` is client-controlled, `sanitize_chat_id`
(`models.py:25-41`) keeps only `[A-Za-z0-9._-]`, collapses the rest to `-`, strips leading/trailing
`-.`, caps at 64 chars, and falls back to a hash for empty results — so it can **never** contain a
path separator or be `.`/`..` (no traversal out of `<mount>`). Spec:
`openspec/changes/per-chat-isolation/specs/per-chat-workdirs/spec.md:63-77`.

**Missing-`X-Session-Id` fallback.** No per-chat dir is created and the session opens in
`<data_mount_path>` (legacy single-directory behaviour); in `perChat` mode a single stable "default"
chat pod is used per user with a logged warning
(`chat_bootstrap.py:41-42`, spec `pod-per-chat-mode/spec.md:117-122`).

---

## 5. Pod lifecycle

**Ephemeral, on-demand, bare Pods (`restartPolicy: Never`).** No pre-warming pool; no Job/Deployment.

- **Created on demand:** lazily by `PodManager.get_or_create` on the first `POST /api/terminals`
  (perChat modes: also gated so **reads never create** — `get_terminal_for_user` is lookup-only in
  perChat modes and returns `503` if absent, `main.py:137-141`; this was the fix for the
  5 s `/ports` poll thrashing pods, `openspec/changes/peruserperchat-stability/proposal.md:12-18`).
- **Destroyed:**
  - **Idle timeout** — `_cleanup_loop` scans every `pod_cleanup_interval_seconds` (default 60s);
    chat pods (`is_chat_pod`) use `chat_pod_idle_timeout_seconds` (default **300s**), user pods use
    `pod_idle_timeout_seconds` (default **3600s**) (`pod_manager.py:371-399`, `config.py:118-143`).
  - **Never evicted while connected** — `active_connections` is incremented by `acquire()` on WS
    open and decremented by `release()`; idle cleanup, global cap eviction, and per-user cap eviction
    all **skip** pods with `active_connections > 0` (`pod_manager.py:185-193`, `:221-238`, `:330-345`,
    `:386`).
  - **Global pod cap** `max_concurrent_pods` (default 100) — evicts oldest *idle* pod
    (`_evict_oldest`, `pod_manager.py:239-240`, `:330-345`).
  - **Per-user pod cap** `max_pods_per_user` (default 5, perChat modes only) — evicts the user's
    oldest *idle* chat pod; if all are connected, raises `RuntimeError` → `503`
    (`pod_manager.py:219-238`).
  - **Health check** every 30s marks pods in `Failed`/`Unknown` for deletion and clears the per-chat
    cwd cache when a container `restart_count` increases or pod IP changes
    (`pod_manager.py:401-457`).
  - **On chat close:** *not* wired in the current code (Open WebUI `chat.deleted` is listed as a
    future hook in `peruserperchat-stability/proposal.md`); chats are reclaimed only by idle timeout.
- **Reconcile on proxy restart:** `_reconcile_existing_pods` lists pods by `managed-by` label,
  re-adopts running ones (reading the live API key from the pod's mounted secret to survive
  eviction races), and deletes non-running leftovers (`pod_manager.py:58-100`, `:113-138`).
- **Resource limits per pod** (`config.py:54-70`, `values.yaml:25-33`): CPU 500m req / 1000m limit,
  memory 512Mi req / 4Gi limit, **ephemeral-storage 5Gi req / 5Gi limit** (kubelet-evicts the pod if
  total writable usage — outside the PVC — exceeds this; the only disk protection in `none` storage
  mode, README `README.md:150-156`).
- **Image:** `ghcr.io/open-webui/open-terminal:latest` (`config.py:45-48`, `values.yaml:8-11`),
  pull policy `IfNotPresent`. Terminal container runs the image ENTRYPOINT with `args
  ["run", "--cwd", <cwd>]` (`k8s/pod_builder.py:184-187`).

---

## 6. Network policy / egress

Two layers, only one of which is implemented:

**(a) Static, uniform Helm NetworkPolicy — IMPLEMENTED** (`templates/networkpolicy-terminal.yaml`).
A single namespace-scoped `NetworkPolicy` selects **all** terminal pods by labels
`app: open-terminal-user`, `managed-by: terminal-proxy`. It always restricts **ingress** to the proxy
pod on TCP/8000 (`networkpolicy-terminal.yaml:23-30`). Egress is operator-chosen:
`mode: denyAll` (no egress) or `mode: allowNetworks` (one `ipBlock` allow rule per `allowedCIDR`,
plus optional DNS rule to `kube-dns`). This is a *global* policy — **identical for every user**.

**(b) Per-user egress NetworkPolicy — DESIGNED, NOT IMPLEMENTED**
(`openspec/changes/user-based-network-policy/`). The design (proposal `proposal.md`, design `design.md`,
spec `specs/per-user-egress/spec.md`, tasks `tasks.md` — **all task checkboxes `[ ]` unchecked**) and
the **absence** of `terminal_proxy/k8s/network_policy_builder.py` and of `networkpolicies` in the
proxy's `Role` (`templates/role.yaml:10-18` only grants pods/services/secrets/pvcs) confirm it is not
shipped. Summarising the design as the *intended* model:

- Proxy creates one namespace-scoped `NetworkPolicy` **per terminal pod**, named
  `terminal-netpol-<user_hash>`, whose `podSelector` matches that pod's `user-id-hash` label
  (`design.md:42-48`, spec `specs/per-user-egress/spec.md:7-11`).
- Rules sourced from a **ConfigMap-mounted JSON map** keyed by `user_id` (path
  `NETWORK_POLICY_CONFIG_PATH`; empty ⇒ feature off). Shape
  `{"default": {allowedCIDRs, deniedCIDRs, dns}, "users": {"alice@example.com": {...}}}`
  (`design.md:58-67`, spec `:38-66`).
- **Deny via `ipBlock.except`** — K8s NetworkPolicy has no native deny, so each `deniedCIDR` becomes
  an `except` entry on every allow CIDR that is its supernet; a deny outside any allow is a no-op;
  a deny that is a *supernet* of an allow logs a warning (`design.md:69-85`, spec `:68-100`).
- **DNS per user** mirrors the Helm DNS selectors (`design.md:108-112`, spec `:102-120`).
- **Additive model** — per-user policies only *widen* egress; unlisted users stay locked down **only
  if the base Helm policy is `denyAll`** (`design.md:101-106`, spec `:142-155`).
- **Lifecycle tied to pod** — created on spawn, deleted on teardown, rebuilt on reconcile using a new
  `user-id` *annotation* (annotations, not labels, to avoid PII/length issues) so reconcile can
  recover the `user_id` that the one-way `user-id-hash` cannot (`design.md:87-99`, spec `:122-141`).
- RBAC: proxy `Role` would gain `get/list/watch/create/delete` on `networkpolicies`
  (`design.md:130-131`, spec `:157-167`) — **not yet in `templates/role.yaml`**.

This is the per-user egress capability the new runtime should *plan to* reproduce, even though the
k8s proxy has not shipped it.

---

## 7. Identity mapping

**There is no email/UPN join in this repo.** Despite the task brief mentioning "a design about
email-based identity mapping," a full search of the repo (`grep -rin "email|UPN|X-OpenWebUI|identity"`
across `*.py/*.md/*.yaml`) returns nothing. Identity resolution is a single deterministic step:

```
X-User-Id  (header, whatever string Open WebUI sends — typically the OWUI user id)
    │
    ▼  user_id_to_hash  (models.py:15-17)
user_hash = sha256(X-User-Id)[:12]   # 12-char hex
    │
    ▼  used as
  • pod / service / secret name suffix   (models.py:111-131)
  • pod label `user-id-hash`             (k8s/pod_builder.py:93-99)
  • storage subPath on the shared PVC    (k8s/pod_builder.py:128-129)
  • NetworkPolicy podSelector (designed) (user-based-network-policy spec.md:7-11)
```

`user_hash` is **one-way**: reconcile cannot recover `user_id` from the hash label, which is exactly
why the per-user-netpol design adds a `user-id` *annotation* (`design.md:87-99`).
**Fallback:** if `X-User-Id` is missing the proxy returns `400` (`main.py:106-111`); there is no
"anonymous" fallback for terminal endpoints. (The unrelated `verify_api_key` does allow
`"anonymous"` only when `PROXY_API_KEY` is unset, `main.py:99-100`.)

For the Cloudflare replacement: treat the `X-User-Id` header (or a signed JWT claim extracted into it)
as the canonical tenant identity, and decide up-front whether you need an email/UPN→runtime mapping
that *this* repo does not provide.

---

## 8. Config / env

From `terminal_proxy/config.py` (Pydantic `BaseSettings`, env-loaded) and `values.yaml`:

**Proxy auth / networking**

- `PROXY_API_KEY` (`config.py:33-36`) — bearer token Open WebUI must present; auto-generated if empty.
- `PROXY_HOST` / `PROXY_PORT` (`:37-38`, defaults `0.0.0.0:8000`).
- `CORS_ALLOWED_ORIGINS` (`:156-158`, default `*`).
- `NAMESPACE` (`:40-43`, default `default`) — k8s namespace for all created objects.

**Terminal pod image / resources**

- `TERMINAL_IMAGE` (`:45-48`, `ghcr.io/open-webui/open-terminal:latest`), `TERMINAL_IMAGE_PULL_POLICY`.
- `TERMINAL_CPU_REQUEST/LIMIT` (500m/1000m), `TERMINAL_MEMORY_REQUEST/LIMIT` (512Mi/4Gi),
  `TERMINAL_EPHEMERAL_STORAGE_REQUEST/LIMIT` (5Gi/5Gi) — `:54-70`.
- `TERMINAL_NODE_SELECTOR`, `TERMINAL_TOLERATIONS` — `:77-82`.

**Storage / isolation mode**

- `STORAGE_MODE` (`:84-87`): `none|perUser|shared|sharedRWO`.
- `STORAGE_CLASS_NAME`, `STORAGE_PER_USER_SIZE` (5Gi), `STORAGE_SHARED_SIZE` (100Gi),
  `STORAGE_RETAIN_PVC`, `STORAGE_PVC_RETENTION_TTL_SECONDS` — `:88-107`.
- `DATA_MOUNT_PATH` (`:130-133`, default `/data`) — PVC mount path **and** `--cwd`/`HOME` target.
- `POD_MODE` (`:134-139`): `perUser|perChat|perUserPerChat` (startup validates compatibility,
  `config.py:167-181`).
- `PER_CHAT_DIRS_ENABLED` (`:144-147`, default `true`).
- `CHAT_POD_IDLE_TIMEOUT_SECONDS` (`:140-143`, default 300).

**Lifecycle / caps**

- `MAX_CONCURRENT_PODS` (100), `MAX_PODS_PER_USER` (5) — `:109-117`.
- `POD_IDLE_TIMEOUT_SECONDS` (3600), `CHAT_POD_IDLE_TIMEOUT_SECONDS` (300),
  `POD_STARTUP_TIMEOUT_SECONDS` (60), `POD_CLEANUP_INTERVAL_SECONDS` (60) — `:118-129`.

**Labels**

- `LABELS_APP` (`open-terminal-user`), `LABELS_MANAGED_BY` (`terminal-proxy`) — `:149-154`; must
  match the Helm `terminalNetworkPolicy.podLabels`.

**Per-user egress (DESIGNED, not yet wired):** `NETWORK_POLICY_CONFIG_PATH` + DNS-selector settings
(`openspec/changes/user-based-network-policy/tasks.md:5-8`).

**K8s RBAC required** (`templates/role.yaml`): `get/list/watch/create/delete` on `pods, services,
secrets`; `+patch` on `persistentvolumeclaims`; `get` on `pods/status`. (Per-user netpol would add
`networkpolicies` in `networking.k8s.io`.)

---

## 9. Isolation requirements (must-replicate in new runtime)

The concrete invariants the Cloudflare-"computer" replacement **MUST** satisfy to match (or improve
on) the current k8s design. These double as acceptance criteria.

- **Per-user tenant separation, keyed by a single propagated identity.** Each request carries a user
  identity (today `X-User-Id`); the runtime must route every request for a user to **only that user's**
  compute instance and storage. No user may reach, list, read, or execute against another user's
  instance or files. (Today: separate pod + Service + PVC/subPath per `user_hash`,
  `pod_manager.py:165-171`, `k8s/pod_builder.py:104-130`.)
- **Per-user authentication credential.** Each user's runtime instance must have its own secret
  (today a per-pod `Secret` + `OPEN_TERMINAL_API_KEY`), and the proxy must authenticate every caller
  (today `PROXY_API_KEY` bearer, `main.py:95-103`). Credentials must rotate on instance recreation.
- **Per-chat working directory (`--cwd` pinning), always on.** Every chat (identified by
  `X-Session-Id`) must resolve relative paths inside its **own** `<data_root>/<sanitized-chatid>`
  directory, distinct from every other chat of the same user. (Today: `chat_bootstrap.py` for
  `perUser`, `--cwd <mount>/<chatid>` + initContainer `mkdir` for perChat,
  `k8s/pod_builder.py:137-210`; spec `per-chat-workdirs/spec.md:29-61`.)
- **Client-controlled chat id is sanitised to a single safe path component.** The chat directory name
  must never contain a path separator or be `.`/`..` — no traversal outside `<data_root>`.
  (Today: `sanitize_chat_id`, `models.py:25-41`; spec `:63-77`.)
- **Deterministic terminal home inside persistent storage.** The terminal process must open *inside*
  the persistent data volume (`HOME == --cwd == <mount>` or `<mount>/<chatid>`), so writes survive
  instance restarts and are not lost to the container's ephemeral layer. (Today:
  `k8s/pod_builder.py:163-187`; spec `per-chat-workdirs/spec.md:5-27`.)
- **Per-user / per-chat persistent filesystem (or explicit ephemeral-with-limits mode).** Either a
  dedicated volume per user (`perUser`) or a shared volume with per-user `subPath`, plus a per-chat
  subdir. If ephemeral, a hard ephemeral-storage cap must bound runaway writes (today 5Gi kubelet
  limit, `config.py:60-70`). (Today: `storage.py`, `k8s/pod_builder.py:104-130`.)
- **No cross-user file visibility.** A user's runtime may mount only that user's PVC (or that user's
  `subPath` of a shared volume). The filesystem boundary is enforced at mount time, not by app logic.
- **On-demand, ephemeral instances with bounded concurrency.** Instances are created lazily on first
  use and destroyed after an idle timeout; a global cap and a per-user cap bound total concurrency,
  evicting the oldest **idle** instance (never one with an active connection). (Today:
  `pod_manager.py:195-255`, `:381-399`; `config.py:109-143`.)
- **Never tear down an in-use instance.** Active websocket/streaming connections must pin the
  instance alive (today `acquire`/`release` + `active_connections`, `pod_manager.py:185-193`, and
  every eviction path skips `active_connections > 0`). Reads/polls must **not** recreate instances
  (the churn fix, `main.py:137-141`).
- **Per-user egress policy (least privilege), default-deny baseline.** Egress must be controllable
  per user (allow-list of CIDRs + deny carve-outs + optional DNS), sourced from a single
  operator-editable rules map, with an additive-on-default-deny model so unlisted users stay locked
  down. (Today: **designed but not shipped** — `openspec/changes/user-based-network-policy/`. The new
  runtime should treat this as a first-class requirement, not an option.)
- **Ingress restricted to the proxy only.** Only the proxy/control plane may reach instance ports;
  direct instance-to-instance or external-to-instance ingress is denied. (Today: static Helm
  NetworkPolicy ingress rule, `templates/networkpolicy-terminal.yaml:23-30`.)
- **Graceful missing-chat fallback.** When `X-Session-Id` is absent, the request must still work
  (single shared/default workspace for that user) rather than fail; logged, not rejected. (Today:
  `chat_bootstrap.py:41-42`; spec `pod-per-chat-mode/spec.md:117-122`.)
- **Survive control-plane restart.** Existing running instances must be re-adopted on restart (not
  duplicated or killed), keyed by stable labels/annotations; auth keys must be recovered from the
  running instance to avoid desync. (Today: `_reconcile_existing_pods` + `_api_key_from_pod`,
  `pod_manager.py:58-100`, `:113-138`.)
- **Bounded, configurable resource usage per instance.** CPU / memory / ephemeral-disk limits per
  instance must be configurable and enforced by the runtime. (Today: k8s resource requests/limits,
  `config.py:54-70`.)
- **(Optional, stronger) Full process isolation per chat.** Where the k8s design offers it as an
  opt-in (`perChat`/`perUserPerChat`: one process tree + environment per chat, so background
  processes and env state do not leak between chats of the same user), the new runtime should prefer
  this as the default if cheap enough. (Today: `pod_builder.py:201-210`; spec
  `pod-per-chat-mode/spec.md:45-59`.)
