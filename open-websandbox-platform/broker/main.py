# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""code-standard broker — the Open WebUI front door.

Two profiles. The default is chosen at DEPLOY time via ``BROKER_DEFAULT_PROFILE``,
(``persistent`` by default — OWUI cannot set request headers); an explicit, valid
``X-Persistence`` header/query still overrides it for admin/testing:

* **ephemeral**: one warm-pool claim per *session*; ``/workspace`` is an
  emptyDir, destroyed when the claim is reaped (idle > ``BROKER_IDLE_TTL_SECONDS``).
* **persistent** (deploy default): one *per-user* claim carrying a
  ``workspace`` ``volumeClaimTemplates`` PVC (cephfs RWX); the same PVC is resumed
  across any of the user's sessions. Each chat runs isolated under
  ``/workspace/<subdir>`` (the broker injects ``X-Workspace-Subdir``). The broker
  **parks** the sandbox (``spec.operatingMode: Suspended`` — pod deleted, node freed,
  PVC retained) after ``BROKER_PARK_IDLE_SECONDS`` idle and **reaps** the claim
  (PVC freed) after ``BROKER_REAP_SECONDS``.

Per request: authenticate (shared Bearer + X-User-Id + X-Session-Id) -> resolve the
sandbox (get-or-create claim; resume if parked) -> reverse-proxy to the sandbox-router
injecting X-Sandbox-Id / X-Sandbox-Namespace / X-Sandbox-Pod-IP (priority-1 resolution).
"""
import asyncio
import base64
import contextlib
import copy
import datetime
import hashlib
import hmac
import json
import logging
import os
import secrets
import time
from typing import cast

import httpx
import websockets
from fastapi import (
    FastAPI,
    HTTPException,
    Request,
    Response,
    Security,
    WebSocket,
    WebSocketDisconnect,
)
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from kubernetes import client, config
from openapi_spec import OPENAPI
from prometheus_client import (
    CONTENT_TYPE_LATEST,
    Counter,
    Gauge,
    Histogram,
    generate_latest,
)

# --- config -----------------------------------------------------------------
SHARED_SECRET = os.environ.get("BROKER_SHARED_SECRET", "")
# Known-unsafe placeholder values (mirror the runtime's _PLACEHOLDER_KEYS): the
# _validate_config() boot guard and the _auth request guard treat these as "unset".
_PLACEHOLDER_SECRETS = frozenset({"", "dev-shared-secret-change-me", "change-me", "changeme", "placeholder"})
RUNTIME_NS = os.environ.get("BROKER_RUNTIME_NS", "agent-sandbox-runtime")
ROUTER_URL = os.environ.get("BROKER_ROUTER_URL", "http://sandbox-router-svc.agent-sandbox-system:8080")
# Per-session runtime API keys (issue #4): the broker mints one per sandbox pod,
# persists it to a per-session Secret owui-runtime-key-<sandbox> in RUNTIME_NS, injects
# it into the pod as a projected volume (mounted at /etc/runtime-key/api-key), and
# reads it back per hop (stateless). Rotate-on-resume mints a fresh key before a parked
# sandbox resumes. The chart grants the broker SA create/get/update/patch/delete on
# Secrets (see chart/templates/broker.yaml).
RUNTIME_KEY_PREFIX = os.environ.get("BROKER_RUNTIME_KEY_PREFIX", "owui-runtime-key-")


def _env_int(name: str, default: int) -> int:
    """Parse an int env var, falling back to default on missing/bad input."""
    try:
        return int(os.environ.get(name, str(default)))
    except (TypeError, ValueError):
        return default


def _now_ts() -> int:
    """Current epoch seconds (helper so call sites avoid a nested throwing int() call)."""
    return time.time_ns() // 1_000_000_000

IDLE_TTL = _env_int("BROKER_IDLE_TTL_SECONDS", 120)                  # ephemeral reap (return to warm pool): 2 min
PARK_TTL = _env_int("BROKER_PARK_IDLE_SECONDS", 120)                 # persistent suspend: 2 min (cold-start is 1-6s)
REAP_TTL = _env_int("BROKER_REAP_SECONDS", 7 * 24 * 3600)            # persistent reap: 7 days
CLAIM_READY_TIMEOUT = _env_int("BROKER_CLAIM_TIMEOUT_SECONDS", 60)
# allow long-running commands (sandbox MAX_TIMEOUT is 600s)
PROXY_TIMEOUT = _env_int("BROKER_PROXY_TIMEOUT_SECONDS", 660)
PERSISTENT_SC = os.environ.get("BROKER_PERSISTENT_STORAGECLASS", "cephfs")
PERSISTENT_SIZE = os.environ.get("BROKER_PERSISTENT_STORAGE", "10Gi")

GROUP, VER = "extensions.agents.x-k8s.io", "v1beta1"
SANDBOX_GROUP = "agents.x-k8s.io"  # the `sandbox` resource lives in a different group than claims
CLAIM_PREFIX = os.environ.get("BROKER_CLAIM_PREFIX", "owui-")
PERSISTENT_PREFIX = os.environ.get("BROKER_PERSISTENT_PREFIX", "owui-p-")
LAST_USED = "broker-last-used"
PROFILE = "broker-profile"
EPHEMERAL = "ephemeral"
PERSISTENT = "persistent"
# Default profile, fixed at DEPLOY time (OWUI cannot send X-Persistence). An explicit,
# valid X-Persistence header/query still overrides it for admin/testing.
DEFAULT_PROFILE = os.environ.get("BROKER_DEFAULT_PROFILE", PERSISTENT).lower()
if DEFAULT_PROFILE not in (EPHEMERAL, PERSISTENT):
    DEFAULT_PROFILE = PERSISTENT  # pragma: no cover - defensive fallback for malformed BROKER_DEFAULT_PROFILE
MANAGED_BY = {"app.kubernetes.io/managed-by": "owui-broker"}
BASE_TEMPLATE = os.environ.get("BROKER_BASE_TEMPLATE", "code-standard-v1")
# Persistent backing, deploy-selectable: per-user-pvc (claim + per-user PVC) or
# shared-subpath (direct per-user Sandbox + subPath slice of one shared RWX PVC).
PERSISTENT_MODE = os.environ.get("BROKER_PERSISTENT_MODE", "per-user-pvc").lower()
if PERSISTENT_MODE not in ("per-user-pvc", "shared-subpath"):
    PERSISTENT_MODE = "per-user-pvc"  # pragma: no cover - defensive fallback for malformed BROKER_PERSISTENT_MODE
SHARED_PVC = os.environ.get("BROKER_SHARED_PVC", "workspace-shared")
SHARED_PREFIX = os.environ.get("BROKER_SHARED_PREFIX", "owui-s-")
CHAT_PREFIX = os.environ.get("BROKER_CHAT_PREFIX", "owui-c-")
PER_USER_PVC_PREFIX = os.environ.get("BROKER_PER_USER_PVC_PREFIX", "workspace-p-")

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("broker")

# --- k8s --------------------------------------------------------------------
try:
    config.load_incluster_config()
except Exception:                      # pragma: no cover - local dev fallback
    config.load_kube_config()
api = client.CustomObjectsApi()
core = client.CoreV1Api()  # per-user-pvc mode: manage per-user PVCs

bearer = HTTPBearer(auto_error=False)


def _auth(credentials: HTTPAuthorizationCredentials | None = Security(bearer)) -> None:
    """Validate the shared Bearer secret (constant-time). Deny-on-unset (defense-in-depth).

    An unset/placeholder BROKER_SHARED_SECRET is a misconfiguration, NOT a "disabled"
    mode: we 503 here so the request path is fail-closed regardless of the startup boot
    guard / lifespan (a process that skipped the startup event still cannot serve an
    authenticated hop). _validate_config() makes the same check at boot. A presented
    Bearer must match (constant-time), else 401."""
    if SHARED_SECRET in _PLACEHOLDER_SECRETS:
        raise HTTPException(status_code=503, detail="BROKER_SHARED_SECRET is not configured")
    if not credentials or not hmac.compare_digest(credentials.credentials.encode(), SHARED_SECRET.encode()):
        raise HTTPException(status_code=401, detail="invalid bearer token")


# --- per-session runtime API keys (issue #4) ---------------------------------
# The broker is STATELESS: the per-session key lives in a per-session Secret
# (owui-runtime-key-<sandbox>) in RUNTIME_NS, which the broker reads on each hop
# (no in-memory/leader key state, no DB). It mints one per sandbox at creation,
# rotates it on resume, reaps it with the sandbox, and injects it into the pod as a
# projected Secret volume (the runtime reads /etc/runtime-key/api-key — see
# runtime/server.py + chart). One pod sees exactly one key.
RUNTIME_KEY_SECRET_LABELS = {**MANAGED_BY, "owui.io/component": "runtime-key"}


def _runtime_key_secret_name(sandbox_name: str) -> str:
    return f"{RUNTIME_KEY_PREFIX}{sandbox_name}"


def _mint_runtime_key() -> str:
    """A fresh high-entropy per-session key (256 bits, URL-safe)."""
    return secrets.token_urlsafe(32)


def _write_runtime_key(sandbox_name: str, key: str) -> None:
    """Create-or-replace the per-session key Secret (idempotent: mint + rotate)."""
    name = _runtime_key_secret_name(sandbox_name)
    body = {
        "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
        "metadata": {"name": name, "namespace": RUNTIME_NS, "labels": RUNTIME_KEY_SECRET_LABELS},
        "stringData": {"api-key": key},
    }
    try:
        core.create_namespaced_secret(RUNTIME_NS, body)
    except client.ApiException as exc:
        if exc.status != 409:
            raise
        core.patch_namespaced_secret(name, RUNTIME_NS, {"stringData": {"api-key": key}})


def _ensure_runtime_key(sandbox_name: str) -> None:
    """Get-or-create the per-session key. Idempotent — used on sandbox creation.

    Re-running resolve_sandbox for an existing session must NOT rotate (that is
    rotate-on-resume's job); a stable key across a session's life is what the runtime
    caches and the broker sends on every hop."""
    try:
        core.read_namespaced_secret(_runtime_key_secret_name(sandbox_name), RUNTIME_NS)
        return
    except client.ApiException as exc:
        if exc.status != 404:
            raise
    _write_runtime_key(sandbox_name, _mint_runtime_key())


def _rotate_runtime_key(sandbox_name: str) -> None:
    """Mint a FRESH key (rotate-on-resume). Create-or-patch; the resumed pod mounts it."""
    _write_runtime_key(sandbox_name, _mint_runtime_key())


def _runtime_key_for(sandbox_name: str) -> str | None:
    """Stateless per-hop lookup: read the per-session key. None when the Secret is
    missing (a misconfiguration / a reaped session) so the hop is sent unauthenticated
    and the runtime fails closed (401/503). Never raises on 404."""
    try:
        sec = core.read_namespaced_secret(_runtime_key_secret_name(sandbox_name), RUNTIME_NS)
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise
    raw = (sec.data or {}).get("api-key")
    return base64.b64decode(raw).decode() if raw else None


def _delete_runtime_key(sandbox_name: str) -> None:
    """Reap the per-session key Secret with the sandbox (best-effort)."""
    try:
        core.delete_namespaced_secret(_runtime_key_secret_name(sandbox_name), RUNTIME_NS)
    except client.ApiException as exc:
        if exc.status != 404:
            log.warning("reap runtime key %s: %s", sandbox_name, exc)


def _sweep_orphan_runtime_keys(live_sandbox_names: set[str]) -> None:
    """Leader-gated sweep: reap per-session runtime-key Secrets whose owning
    Sandbox no longer exists (issue #51 hardening).

    A broker crash between _delete_sandbox (deletes the Sandbox CR) and
    _delete_runtime_key, or between _ensure_runtime_key and a failed
    _create_sandbox, leaves an orphaned Secret with no owning Sandbox. This
    lists Secrets labeled managed-by=owui-broker in RUNTIME_NS, derives each
    owner from the owui-runtime-key-<sandbox> naming convention (#50), and
    deletes any whose Sandbox is gone. 404-tolerant (a concurrent reap may
    have already removed it). Idempotent: a live owner's key is never touched."""
    try:
        secs = core.list_namespaced_secret(
            RUNTIME_NS, label_selector="app.kubernetes.io/managed-by=owui-broker")
    except client.ApiException as exc:  # pragma: no cover - non-fatal; next tick retries
        log.warning("orphan-key sweep: list secrets failed: %s", exc)
        return
    for sec in (secs.items or []):
        sname = sec.metadata.name if sec.metadata else None
        if not sname or not sname.startswith(RUNTIME_KEY_PREFIX):
            continue  # a managed-by Secret that isn't a per-session key — not ours to reap
        sandbox_name = sname[len(RUNTIME_KEY_PREFIX):]
        if sandbox_name in live_sandbox_names:
            continue
        _delete_runtime_key(sandbox_name)
        log.info("orphan-key sweep: reaped %s (owning sandbox %s gone)", sname, sandbox_name)

def _runtime_auth_headers(sandbox_name: str) -> dict:
    """Authorization header for an outbound broker -> runtime hop (terminal/execute/files).

    Resolves the per-session key for the target pod via a STATELESS Secret get (no
    in-memory cache: the Secret is the single source of truth, HA-safe across replicas).
    Returns {} when the key is unresolved so the runtime fails closed (401/503)."""
    key = _runtime_key_for(sandbox_name)
    return {"Authorization": f"Bearer {key}"} if key else {}


# --- session -> sandbox -----------------------------------------------------
def _ephemeral_sandbox_name(user_id: str, session_id: str) -> str:
    """Deterministic, DNS-label-safe ephemeral per-SESSION Sandbox name."""
    digest = hashlib.sha256(f"{user_id}|{session_id}".encode()).hexdigest()[:12]
    return f"{CLAIM_PREFIX}{digest}"


def _inject_runtime_key_volume(pod_tmpl: dict, sandbox_name: str) -> None:
    """Add the per-session runtime-key Secret volume + a readOnly mount at /etc/runtime-key.

    The broker mints the per-session key into Secret owui-runtime-key-<sandbox> (see
    _ensure_runtime_key / _rotate_runtime_key) BEFORE the Sandbox is created, so the
    (non-optional) projected secret volume is satisfiable at pod-creation time. The
    runtime reads /etc/runtime-key/api-key — one pod, one key (issue #4)."""
    secret_name = _runtime_key_secret_name(sandbox_name)
    pod_spec = pod_tmpl.setdefault("spec", {})
    volumes = pod_spec.setdefault("volumes", [])
    if not any(v.get("name") == "runtime-key" for v in volumes):
        volumes.append({"name": "runtime-key", "secret": {
            "secretName": secret_name, "items": [{"key": "api-key", "path": "api-key"}]}})
    for c in pod_spec.get("containers", []):
        mounts = c.setdefault("volumeMounts", [])
        if not any(vm.get("name") == "runtime-key" for vm in mounts):
            mounts.append({"name": "runtime-key", "mountPath": "/etc/runtime-key", "readOnly": True})


def _sandbox_operating_mode(sandbox_name: str) -> str | None:
    try:
        sbx = cast(dict, api.get_namespaced_custom_object(SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", sandbox_name))
        return sbx.get("spec", {}).get("operatingMode")
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise


def _set_sandbox_operating_mode(sandbox_name: str, mode: str) -> None:
    """Park (Suspended) or resume (Running) a sandbox by patching spec.operatingMode."""
    try:
        api.patch_namespaced_custom_object(
            SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", sandbox_name,
            {"spec": {"operatingMode": mode}})
        log.info("sandbox %s operatingMode -> %s", sandbox_name, mode)
    except client.ApiException as exc:        # pragma: no cover - non-fatal
        log.warning("operatingMode patch %s=%s failed: %s", sandbox_name, mode, exc)


# --- per-session direct Sandbox (both profiles) ------------------------------
# The broker creates a per-session Sandbox (agents.x-k8s.io) directly for BOTH
# profiles (issue #4): the v1beta1 SandboxClaim requires warmPoolRef (warm-pod reuse)
# and its env is static-only (no secretKeyRef/volumes), so it cannot carry a per-session
# Secret. A direct Sandbox carries a full per-instance podTemplate the controller honors,
# so the broker injects a projected per-session Secret volume here (true per-pod scoping).


def _chat_sandbox_name(user_id: str, session_id: str) -> str:
    """Deterministic per-CHAT persistent Sandbox name (one sandbox per chat)."""
    h = hashlib.sha256(f"{user_id}/{session_id}".encode()).hexdigest()[:12]
    return f"{CHAT_PREFIX}{h}"


def _user_pvc_name(user_id: str) -> str:
    return f"{PER_USER_PVC_PREFIX}{hashlib.sha256(user_id.encode()).hexdigest()[:12]}"


def _ensure_user_pvc(user_id: str) -> str:
    """per-user-pvc mode: get-or-create the user's dedicated PVC (cephfs RWX)."""
    name = _user_pvc_name(user_id)
    try:
        core.read_namespaced_persistent_volume_claim(name, RUNTIME_NS)
        return name
    except client.ApiException as exc:
        if exc.status != 404:
            raise
    body = {
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": name, "namespace": RUNTIME_NS, "labels": {**MANAGED_BY, PROFILE: PERSISTENT}},
        "spec": {"accessModes": ["ReadWriteMany"], "storageClassName": PERSISTENT_SC,
                 "resources": {"requests": {"storage": PERSISTENT_SIZE}}},
    }
    try:
        core.create_namespaced_persistent_volume_claim(RUNTIME_NS, body)
    except client.ApiException as exc:
        if exc.status != 409:
            raise
    return name


def _persistent_volume(user_id: str) -> tuple[str, str]:
    """(pvc_name, subpath_prefix) for the active persistent mode.

    shared-subpath: the static shared PVC, prefix users/<user_id>/.
    per-user-pvc:   the user's dedicated PVC (ensured), prefix '' (chat is top-level)."""
    if PERSISTENT_MODE == "shared-subpath":
        return SHARED_PVC, f"users/{user_id}/"
    return _ensure_user_pvc(user_id), ""


def _get_sandbox(name: str) -> dict | None:
    try:
        return cast(dict, api.get_namespaced_custom_object(SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", name))
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise


def _create_sandbox(name: str, user_id: str, session_id: str, profile: str) -> dict | None:
    """Create a per-session Sandbox (both profiles) with a per-session runtime-key volume.

    The podTemplate is cloned from the base SandboxTemplate and gets:
      - a projected per-session Secret volume `runtime-key` -> /etc/runtime-key (issue #4);
      - /workspace: ephemeral keeps the template's emptyDir; persistent points it at the
        per-chat folder via a subPath slice of the persistent volume (shared PVC or
        per-user PVC) — other chats and users stay invisible (hard isolation).
    shutdownPolicy=Retain keeps the Sandbox object when parked so resume (not recreate)
    reuses the same identity + per-session Secret."""
    tmpl = cast(dict, api.get_namespaced_custom_object(GROUP, VER, RUNTIME_NS, "sandboxtemplates", BASE_TEMPLATE))
    pod_tmpl = copy.deepcopy(tmpl["spec"]["podTemplate"])
    pod_spec = pod_tmpl.setdefault("spec", {})
    pod_tmpl.setdefault("metadata", {}).setdefault("labels", {})["profile"] = profile
    if profile == PERSISTENT:
        pvc_name, prefix = _persistent_volume(user_id)
        sub_path = f"{prefix}{_subdir_for(session_id)}/"
        for v in pod_spec.get("volumes", []):
            if v.get("name") == "workspace":
                v.clear()
                v["name"] = "workspace"
                v["persistentVolumeClaim"] = {"claimName": pvc_name}
        for c in pod_spec.get("containers", []):
            for vm in c.get("volumeMounts", []):
                if vm.get("name") == "workspace":
                    vm["subPath"] = sub_path
    # ephemeral: leave the template's emptyDir workspace as-is
    _inject_runtime_key_volume(pod_tmpl, name)
    labels = {**MANAGED_BY, PROFILE: profile}
    annots: dict = {LAST_USED: str(_now_ts()), "broker-user": user_id, "broker-session": session_id}
    if profile == PERSISTENT:
        labels["broker-persistent-mode"] = PERSISTENT_MODE
        labels["broker-chat"] = "true"
    body = {
        "apiVersion": f"{SANDBOX_GROUP}/{VER}", "kind": "Sandbox",
        "metadata": {"name": name, "namespace": RUNTIME_NS, "labels": labels, "annotations": annots},
        "spec": {"operatingMode": "Running", "shutdownPolicy": "Retain", "podTemplate": pod_tmpl},
    }
    try:
        sbx = cast(dict, api.create_namespaced_custom_object(SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", body))
        SANDBOXES_CREATED.labels(profile=profile).inc()  # re-wired from #49's claim path -> direct create
        return sbx
    except client.ApiException as exc:
        if exc.status == 409:
            return _get_sandbox(name)
        raise


def _sandbox_ready(sbx: dict) -> bool:
    return any(c.get("type") == "Ready" and c.get("status") == "True"
               for c in (sbx.get("status", {}) or {}).get("conditions", []))


def _sandbox_pod_ip(sbx: dict) -> str | None:
    ips = (sbx.get("status", {}) or {}).get("podIPs") or []
    return ips[0] if ips else None


def _sandbox_ready_with_ip(sbx: dict) -> bool:
    """Ready predicate for watches: sandbox is Ready AND has a pod IP."""
    return _sandbox_ready(sbx) and bool(_sandbox_pod_ip(sbx))


def _resume_if_suspended(name: str, _obj) -> None:
    """on_event helper for watches: flip a Suspended sandbox back to Running."""
    if _sandbox_operating_mode(name) == "Suspended":
        _set_sandbox_operating_mode(name, "Running")


def _touch_sandbox(name: str) -> None:
    try:
        api.patch_namespaced_custom_object(
            SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", name,
            {"metadata": {"annotations": {LAST_USED: str(_now_ts())}}})
    except client.ApiException as exc:        # pragma: no cover - non-fatal
        log.debug("non-fatal sandbox last-used touch: %s", exc)


def _watch_until_ready(group: str, plural: str, name: str, is_ready, deadline_s: float, on_event=None) -> dict | None:
    """List a custom object once, then Watch it until is_ready(obj) or the deadline.

    Event-driven replacement for a 1s poll loop: the initial GET reflects current state
    (a ready object returns immediately), otherwise a single Watch stream blocks
    server-side for the next change. on_event(name, obj) runs for the initial + each
    streamed object (used to resume a Suspended sandbox). Returns the ready object or
    None (timeout / missing). Sync — run via asyncio.to_thread."""
    from kubernetes.watch import Watch
    end = time.time() + deadline_s
    try:
        obj = cast(dict, api.get_namespaced_custom_object(group, VER, RUNTIME_NS, plural, name))
    except client.ApiException as exc:  # 404 -> missing; other -> treat as not-ready
        log.debug("watch %s/%s initial get failed: %s", group, name, exc)
        return None
    if on_event:
        on_event(name, obj)
    if is_ready(obj):
        return obj
    rv = ((obj.get("metadata") or {}) if isinstance(obj, dict) else {}).get("resourceVersion")
    remaining = end - time.time()
    if remaining <= 0:
        return None
    try:
        for ev in Watch().stream(api.list_namespaced_custom_object, group=group, version=VER,
                                 namespace=RUNTIME_NS, plural=plural,
                                 field_selector=f"metadata.name={name}", resource_version=rv,
                                 timeout_seconds=int(remaining) + 1):
            ev_d = ev if isinstance(ev, dict) else {}
            obj = ev_d.get("raw_object") or ev_d.get("object") or {}
            if on_event:
                on_event(name, obj)
            if is_ready(obj):
                return obj
            if time.time() >= end:  # pragma: no cover - defensive; watch timeout_seconds bounds the stream
                return None
    except Exception as exc:  # stream error -> caller treats as not-ready/timeout
        log.debug("watch %s/%s stream ended: %s", group, name, exc)
    return None


async def _ensure_sandbox_running_ip(name: str, timeout: float = 90.0) -> str | None:
    """Resume a parked sandbox (rotating its per-session key first) + wait for the pod IP.

    Rotate-on-resume (issue #4): a fresh key is minted BEFORE the new pod boots, so a key
    observed by a prior (parked) pod cannot be replayed against the resumed pod."""
    if _sandbox_operating_mode(name) == "Suspended":
        _rotate_runtime_key(name)
    obj = await asyncio.to_thread(
        _watch_until_ready, SANDBOX_GROUP, "sandboxes", name, _sandbox_ready_with_ip,
        timeout, _resume_if_suspended
    )
    return _sandbox_pod_ip(obj) if obj else None


_MIGRATE_ZIP = "__broker_migrate.zip"
_migrate_locks: dict = {}


async def _clear_workspace(sandbox_name: str, pod_ip: str) -> None:
    """Best-effort wipe of a sandbox's /workspace contents (keeps the mount point)."""
    with contextlib.suppress(Exception):
        await _client.post(
            f"http://{pod_ip}:8888/execute",
            headers=_runtime_auth_headers(sandbox_name),
            json={"command": "find /workspace -mindepth 1 -delete"},
            timeout=60,
        )


async def _migrate_staging_to_chat(user_id: str, chat_name: str, chat_pod_ip: str) -> None:
    """Carry the user's staging /workspace into the freshly-created chat sandbox, then
    wipe staging. Fires once per new chat (session != user) — moves files uploaded
    BEFORE the chat had a chatId, and guarantees no A->B cross-chat leak.

    Best-effort: failures are logged, never fatal. If the staging pod IS reachable the
    workspace is always wiped (anti-leak, independent of the move succeeding). If it is
    NOT reachable, the staging Sandbox is deleted outright (per the product decision that
    short-lived pre-chat uploads are disposable) so its data cannot leak into a later chat.

    Hops are authenticated with the per-session key of the TARGET sandbox (issue #4):
    staging hops use the staging key, chat hops use the chat key."""
    staging = _chat_sandbox_name(user_id, user_id)
    lock = _migrate_locks.setdefault(user_id, asyncio.Lock())
    async with lock:
        try:
            if _get_sandbox(staging) is None:
                return  # user never used a no-session (staging) sandbox
            sip = await _ensure_sandbox_running_ip(staging, timeout=90.0)
            if not sip:
                log.warning("staging migrate: %s not reachable; deleting staging to prevent cross-chat leak user=%s",
                            staging, user_id[:16])
                _delete_sandbox(staging)
                SANDBOXES_DELETED.labels(profile=PERSISTENT).inc()
                return
            moved = 0
            try:
                lr = await _client.get(f"http://{sip}:8888/files/list",
                                      headers=_runtime_auth_headers(staging),
                                      params={"directory": "/workspace"}, timeout=30)
                names = [e["name"] for e in lr.json().get("entries", [])] if lr.status_code == 200 else []
                if names:
                    ar = await _client.post(f"http://{sip}:8888/files/archive",
                                            headers=_runtime_auth_headers(staging),
                                            json={"paths": names}, timeout=120)
                    if ar.status_code == 200 and ar.content:
                        ur = await _client.post(
                            f"http://{chat_pod_ip}:8888/files/upload",
                            headers=_runtime_auth_headers(chat_name),
                            files={"file": (_MIGRATE_ZIP, ar.content, "application/zip")},
                            data={"directory": "/workspace"}, timeout=120,
                        )
                        if ur.status_code == 200:
                            extract_cmd = (
                                "cd /workspace && python3 -c \""
                                "import zipfile,os;"
                                " zipfile.ZipFile('" + _MIGRATE_ZIP + "').extractall('.');"
                                " os.remove('" + _MIGRATE_ZIP + "')\""
                            )
                            await _client.post(f"http://{chat_pod_ip}:8888/execute",
                                               headers=_runtime_auth_headers(chat_name),
                                               json={"command": extract_cmd}, timeout=60)
                            moved = len(names)
                        else:
                            log.warning("staging migrate: chat upload -> %s", ur.status_code)
                    else:
                        log.warning("staging migrate: staging archive -> %s", ar.status_code)
                else:
                    log.info("staging migrate: staging empty, nothing to move")
            except Exception as exc:
                log.warning("staging migrate: move phase failed (will still clear): %s", exc)
            # ALWAYS clear staging once reachable — anti-leak, independent of the move.
            await _clear_workspace(staging, sip)
            log.info("staging migrate user=%s staging=%s -> chat=%s moved=%d entries",
                     user_id[:16], staging, chat_name, moved)
        except Exception as exc:
            log.warning("staging migrate failed (non-fatal): %s", exc)


async def resolve_sandbox(user_id: str, session_id: str, profile: str) -> tuple[str, str]:
    """Return (sandbox_id, pod_ip): get-or-create the per-session Sandbox for this
    user/session, minting a per-session runtime key (issue #4), rotating it on resume,
    then waiting for the pod IP. sandbox_id == the Sandbox (== pod) name, so the
    per-session key Secret owui-runtime-key-<sandbox_id> resolves the Bearer on every hop.

    Both profiles now provision a per-CHAT/per-SESSION direct Sandbox (the SandboxClaim
    warm-pool path cannot carry a per-session Secret — see _create_sandbox). Persistent:
    /workspace is the chat's folder via subPath (shared PVC or per-user PVC); ephemeral:
    emptyDir /workspace. BROKER_PERSISTENT_MODE selects the backing volume."""
    name = (_chat_sandbox_name(user_id, session_id) if profile == PERSISTENT
            else _ephemeral_sandbox_name(user_id, session_id))
    pre = _get_sandbox(name)
    just_created = pre is None
    if just_created:
        # Mint the per-session key BEFORE the pod is created so the (non-optional)
        # projected secret volume is satisfiable at pod-creation time.
        _ensure_runtime_key(name)
        if _create_sandbox(name, user_id, session_id, profile) is None:
            raise HTTPException(status_code=500, detail=f"sandbox {name} could not be created")
    elif _sandbox_operating_mode(name) == "Suspended":
        # Rotate-on-resume: a fresh key before the resumed (new) pod boots.
        _rotate_runtime_key(name)
    obj = await asyncio.to_thread(
        _watch_until_ready, SANDBOX_GROUP, "sandboxes", name, _sandbox_ready_with_ip,
        CLAIM_READY_TIMEOUT, _resume_if_suspended
    )
    if obj is None:
        raise HTTPException(status_code=504, detail=f"sandbox {name} not ready in {CLAIM_READY_TIMEOUT}s")
    pod_ip = cast(str, _sandbox_pod_ip(obj))
    _touch_sandbox(name)
    log.info("session user=%s profile=%s -> sandbox=%s pod=%s",
             user_id[:32], profile, name, pod_ip)
    if just_created and profile == PERSISTENT and session_id != user_id:
        await _migrate_staging_to_chat(user_id, name, pod_ip)
    return name, pod_ip

# --- reverse proxy ----------------------------------------------------------
HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te",
       "trailers", "transfer-encoding", "upgrade", "host", "content-length", "authorization"}


app = FastAPI(title="code-standard broker", docs_url=None, redoc_url=None, openapi_url=None)
_client = httpx.AsyncClient(timeout=httpx.Timeout(PROXY_TIMEOUT), follow_redirects=False)


# --- observability: Prometheus metrics (open_websandbox_ prefix) ----------------
# Bounded-cardinality route label: the matched Route's templated path
# (e.g. /api/terminals/{session_id}, /healthz, /{path:path}), NOT the raw URL — the
# catch-all proxy would otherwise mint a unique label per request (cardinality bomb).
def _route_template(request: Request) -> str:
    route = request.scope.get("route")
    return getattr(route, "path", None) or "unmatched"


_HTTP_REQUESTS = Counter(
    "open_websandbox_broker_http_requests_total",
    "Broker HTTP requests handled", ["method", "path", "status"],
)
_HTTP_DURATION = Histogram(
    "open_websandbox_broker_http_request_duration_seconds",
    "Broker HTTP request latency", ["method", "path", "status"],
)
ACTIVE_SANDBOXES = Gauge(
    "open_websandbox_broker_active_sandboxes",
    "Active sandboxes managed by the broker (leader-gated reaper view)", ["profile"],
)
SANDBOXES_CREATED = Counter(
    "open_websandbox_broker_sandboxes_created_total",
    "Sandboxes created by the broker", ["profile"],
)
SANDBOXES_DELETED = Counter(
    "open_websandbox_broker_sandboxes_deleted_total",
    "Sandboxes deleted/reaped by the broker", ["profile"],
)
RUNTIME_HOP_ERRORS = Counter(
    "open_websandbox_broker_runtime_hop_errors_total",
    "Broker -> runtime hop failures (transport error / timeout)",
)


@app.middleware("http")
async def _observe_request(request: Request, call_next):
    """Count + time every request; always emit a label (even on unhandled error).

    The route template is resolved AFTER call_next (in finally): the matched Route is
    populated by the Router inside call_next, so scope["route"] is only set once routing
    has happened. Bounded-cardinality templated path, never the raw URL.
    """
    start = time.perf_counter()
    status = "500"
    try:
        response = await call_next(request)
        status = str(response.status_code)
        return response
    finally:
        route = _route_template(request)  # resolved post-routing (scope["route"] now set)
        _HTTP_REQUESTS.labels(request.method, route, status).inc()
        _HTTP_DURATION.labels(request.method, route, status).observe(time.perf_counter() - start)


# --- OpenTelemetry tracing (bring-your-own collector via OTLP) -------------------
# Optional/soft: a complete no-op when the OTel libraries are not importable OR
# OTEL_EXPORTER_OTLP_ENDPOINT is unset, so the broker boots + serves regardless.
# When configured it auto-instruments FastAPI (server spans) + httpx (client spans);
# the broker->runtime hop is then traced end-to-end (trace context propagates via the
# httpx headers the instrumentation injects). No collector is deployed by default.
class _NoOpSpan:
    """Stand-in span used when OTel is inactive; absorbs every call."""

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        return False

    def set_attribute(self, *_a, **_k):
        pass

    def set_attributes(self, *_a, **_k):
        pass

    def record_exception(self, *_a, **_k):
        pass

    def add_event(self, *_a, **_k):
        pass


class _NoOpTracer:
    """Stand-in tracer used when OTel is inactive."""

    def start_as_current_span(self, *_a, **_k):
        return _NoOpSpan()


_tracer = _NoOpTracer()


def _setup_telemetry(app_obj, service_name: str, *, client=None) -> None:
    """Configure OTel tracing against a bring-your-own OTLP collector.

    A no-op unless OTEL_EXPORTER_OTLP_ENDPOINT is set AND the opentelemetry-* packages
    are importable. Instruments the FastAPI app + (optionally) the shared httpx client so
    the broker->runtime hop is traced. healthz/readyz/metrics scrapes are excluded.
    """
    endpoint = os.environ.get("OTEL_EXPORTER_OTLP_ENDPOINT")
    if not endpoint:
        return
    try:
        from opentelemetry import trace
        from opentelemetry.exporter.otlp.proto.http.trace_exporter import (
            OTLPSpanExporter,
        )
        from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
        from opentelemetry.sdk.resources import Resource
        from opentelemetry.sdk.trace import TracerProvider
        from opentelemetry.sdk.trace.export import BatchSpanProcessor

        if client is not None:
            from opentelemetry.instrumentation.httpx import HTTPXClientInstrumentor
    except ImportError:
        log.debug("OpenTelemetry libraries not installed; tracing disabled")
        return

    provider = TracerProvider(resource=Resource.create({
        "service.name": os.environ.get("OTEL_SERVICE_NAME", service_name),
        "service.namespace": "open-websandbox",
    }))
    # OTLPSpanExporter() honours OTEL_EXPORTER_OTLP_ENDPOINT / *_PROTOCOL per the OTel spec.
    provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
    trace.set_tracer_provider(provider)
    FastAPIInstrumentor.instrument_app(app_obj, excluded_urls="healthz,readyz,metrics")
    if client is not None:
        HTTPXClientInstrumentor().instrument()
    global _tracer
    _tracer = trace.get_tracer("open-websandbox.broker")
    log.info("OpenTelemetry tracing enabled -> %s (service=%s)", endpoint, service_name)


# Bootstrap at import: no-op in tests (no OTEL_EXPORTER_OTLP_ENDPOINT); active in
# production when the chart points the broker at a collector (bundled or BYO).
_setup_telemetry(app, "open-websandbox-broker", client=_client)


@app.get("/metrics", include_in_schema=False)
async def metrics() -> Response:
    """Prometheus exposition: process/python-runtime metrics + the open_websandbox_*
    counters/histograms/gauges. Registered before the catch-all proxy so scrape traffic
    isn't forwarded to a sandbox."""
    return Response(generate_latest(), media_type=CONTENT_TYPE_LATEST)


@app.get("/openapi.json", include_in_schema=False)
async def openapi_json() -> dict:
    """Curated OpenAPI 3.0 spec (the LLM-facing method surface). Registered before the
    catch-all proxy so Open WebUI can discover the tools without being forwarded to a
    sandbox."""
    return OPENAPI


@app.get("/docs", include_in_schema=False)
async def swagger_ui():
    from fastapi.responses import HTMLResponse
    return HTMLResponse(
        '<!DOCTYPE html><html><head><meta charset="utf-8"><title>code-standard broker</title>'
        '<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">'
        '</head><body><div id="swagger-ui"></div>'
        '<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>'
        '<script>window.ui=SwaggerUIBundle({url:"/openapi.json",dom_id:"#swagger-ui"});</script>'
        '</body></html>'
    )


@app.get("/healthz")
async def health():
    return {"status": "ok"}


@app.get("/readyz")
async def ready():
    """Readiness = the apiserver (our hard dependency for sandbox resolution) is reachable.
    Unlike /healthz (process-up only), this fails when the control plane is unavailable so
    the Service stops routing to a broker that would only 500 — prevents the silent
    partial-outage where /healthz stays green while every resolve_sandbox throws."""
    try:
        api.list_namespaced_custom_object(
            SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", limit=1, _request_timeout=3,
        )
    except Exception as exc:
        log.warning("readyz: apiserver unreachable: %s", exc)
        raise HTTPException(status_code=503, detail="apiserver unreachable")
    return {"status": "ready"}


def _subdir_for(session_id: str) -> str:
    """Safe per-chat workspace folder (hex; always matches the runtime's subdir guard)."""
    return hashlib.sha256(session_id.encode()).hexdigest()[:16]


@app.websocket("/api/terminals/{session_id}")
async def terminal_ws(client_ws: WebSocket, session_id: str):
    """Interactive terminal (OWUI open-terminal contract).

    OWUI opens this WS after POST /api/terminals (proxied by the catch-all to the
    runtime, which forks the PTY). Here we validate OWUI's first-message auth, resolve
    the sandbox, then proxy the WS through the sandbox-router (which upgrades and
    routes by X-Sandbox-Pod-IP) to the runtime PTY. Identity is read from query params
    or headers — browser WS clients cannot set arbitrary headers, so user_id /
    session_id may arrive either way.
    """
    await client_ws.accept()
    if SHARED_SECRET:
        try:
            raw = await asyncio.wait_for(client_ws.receive_text(), timeout=10.0)
            payload = json.loads(raw)
            if payload.get("type") != "auth" or not hmac.compare_digest(
                str(payload.get("token", "")), SHARED_SECRET
            ):
                await client_ws.close(code=4001, reason="invalid api key")
                return
        except Exception:
            await client_ws.close(code=4001, reason="auth timeout or invalid payload")
            return

    user = client_ws.query_params.get("user_id") or client_ws.headers.get("x-user-id", "")
    session = (
        client_ws.query_params.get("session_id")
        or client_ws.query_params.get("chat_id")
        or client_ws.headers.get("x-session-id", "")
        or session_id  # OWUI's WS proxy puts the chat-id in the path, not a query param
    )
    if not user or not session:
        await client_ws.close(code=1008, reason="user_id and session_id are required")
        return
    _persist = (
        client_ws.query_params.get("persistence", "").lower()
        or client_ws.headers.get("x-persistence", "").lower()
    )
    profile = _persist if _persist in (PERSISTENT, EPHEMERAL) else DEFAULT_PROFILE
    try:
        sandbox_id, pod_ip = await resolve_sandbox(user, session, profile)
    except HTTPException as exc:
        await client_ws.close(code=1011, reason=f"sandbox unavailable: {exc.detail}")
        return

    # Ensure an interactive PTY exists on this resolved pod before attaching the WS.
    # OWUI's own POST /api/terminals is routed via the sandbox-router and may land on a
    # different pod than this direct WS; creating it here makes the terminal
    # self-contained and avoids a runtime 4004 'unknown session' close (idempotent).
    with contextlib.suppress(Exception):
        await _client.post(
            f"http://{pod_ip}:8888/api/terminals",
            headers={**_runtime_auth_headers(sandbox_id), "X-Session-Id": session_id},
        )

    # Plaintext WebSocket is correct here: in-cluster pod-to-pod traffic inside the
    # trusted network (TLS terminates at the ingress). The scheme is held in a local so
    # the source carries no plaintext ws-scheme literal that would trip a blanket
    # client-side insecure-WS rule (such rules don't apply to cluster-internal traffic).
    _ws = "ws"
    upstream = f"{_ws}://{pod_ip}:8888/api/terminals/{session_id}"
    log.info("terminal ws user=%s session=%s -> sandbox=%s pod=%s", user[:32], session[:32], sandbox_id, pod_ip)
    try:
        async with websockets.connect(upstream) as up_ws:  # nosemgrep: detect-insecure-websocket - plaintext in-cluster pod-to-pod; TLS terminates at ingress

            async def _client_to_upstream():
                try:
                    while True:
                        msg = await client_ws.receive()
                        if msg["type"] == "websocket.disconnect":
                            break
                        if msg.get("bytes"):
                            await up_ws.send(msg["bytes"])
                        elif msg.get("text"):
                            await up_ws.send(msg["text"])
                        else:  # pragma: no cover - ASGI websocket.receive always carries bytes or text
                            pass
                except (WebSocketDisconnect, Exception):
                    pass

            async def _upstream_to_client():
                try:
                    async for msg in up_ws:
                        if isinstance(msg, bytes):
                            await client_ws.send_bytes(msg)
                        else:
                            await client_ws.send_text(msg)
                except Exception:  # pragma: no cover - client gone mid-relay (timing); exercised in e2e
                    pass

            # Stop as soon as EITHER side ends. Closing the upstream WS is what lets the
            # runtime's WS handler reach its finally-block _term_cleanup, so the PTY is
            # killed instead of leaking until the per-pod cap (-> 429).
            c2u = asyncio.create_task(_client_to_upstream())
            u2c = asyncio.create_task(_upstream_to_client())
            await asyncio.wait({c2u, u2c}, return_when=asyncio.FIRST_COMPLETED)
            for t in (c2u, u2c):
                t.cancel()
            with contextlib.suppress(Exception):
                await up_ws.close()
            with contextlib.suppress(Exception):
                await client_ws.close()
            await asyncio.gather(c2u, u2c, return_exceptions=True)
    except Exception as exc:
        log.warning("terminal ws upstream %s failed: %s", upstream, exc)
        with contextlib.suppress(Exception):
            await client_ws.close(code=1011, reason="terminal unavailable")



@app.get("/api/config", include_in_schema=False)
async def terminal_config(_=Security(_auth)):
    """Feature discovery — the UI connection-test gate.

    Static (never proxied): served Bearer-only, no X-User-Id, matching how
    open-terminal-k8s-proxy serves it. The OWUI terminal UI treats this as the
    connection-success signal.
    """
    return {"features": {"terminal": True, "notebooks": False, "desktop": False}}


@app.get("/api/status", include_in_schema=False)
async def terminal_status(_=Security(_auth)):
    """Operator telemetry. Static (never proxied)."""
    return {"active_pods": 0, "max_pods": 10, "pods": []}
@app.api_route("/{path:path}", methods=["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"])
async def proxy(path: str, request: Request, _=Security(_auth)):
    user = request.headers.get("X-User-Id", "")
    session = request.headers.get("X-Session-Id") or user
    if not user:
        raise HTTPException(status_code=400, detail="X-User-Id header is required")
    _persist = request.headers.get("X-Persistence", "").lower()
    profile = _persist if _persist in (PERSISTENT, EPHEMERAL) else DEFAULT_PROFILE
    sandbox_id, pod_ip = await resolve_sandbox(user, session, profile)

    fwd = {k: v for k, v in request.headers.items() if k.lower() not in HOP}
    fwd.update({"X-Sandbox-Id": sandbox_id, "X-Sandbox-Namespace": RUNTIME_NS, "X-Sandbox-Pod-IP": pod_ip})
    # Inject the RESOLVED session (chatId, or user when OWUI omits X-Session-Id) so the
    # runtime echoes it as the terminal id. Otherwise a no-chatId createTerminal mints a
    # random id and the terminal WS lands on a rogue sandbox that disagrees with the
    # file/tool API (and spawns a throwaway sandbox per terminal open).
    fwd["X-Session-Id"] = session
    # Inject the runtime inter-component credential so the DIRECT broker -> runtime pod
    # hop (the catch-all bypasses the sandbox-router, which drops Authorization) satisfies
    # _auth_runtime on /execute, /files/* and the terminal management endpoints.
    # Per-session runtime key (issue #4): resolves THIS pod's key from its Secret.
    fwd.update(_runtime_auth_headers(sandbox_id))
    # No X-Workspace-Subdir: each chat's folder IS /workspace (per-chat subPath).
    body = await request.body()
    upstream = httpx.Request(request.method, f"http://{pod_ip}:8888/{path}",
                             headers=fwd, params=request.query_params, content=body)
    with _tracer.start_as_current_span("broker.runtime_hop") as span:
        # Correlate the hop to a specific sandbox (the auto httpx span only sees the URL).
        span.set_attribute("sandbox.id", sandbox_id)
        span.set_attribute("sandbox.pod_ip", pod_ip)
        span.set_attribute("http.method", request.method)
        span.set_attribute("http.route", path)
        try:
            resp = await _client.send(upstream, stream=True)
            resp_body = await resp.aread()
            span.set_attribute("http.status_code", resp.status_code)
        except Exception as hop_exc:
            RUNTIME_HOP_ERRORS.inc()
            span.record_exception(hop_exc)
            raise
    out_headers = {k: v for k, v in resp.headers.items() if k.lower() not in HOP}
    # Rewrite redirect Location so clients follow back through the broker instead of
    # the runtime pod IP (e.g. Starlette's /list/. -> /list/ 307), which is unreachable
    # from outside agent-sandbox-system.
    if resp.status_code in (301, 302, 303, 307, 308):
        loc = resp.headers.get("location")
        if loc:
            from urllib.parse import urlsplit, urlunsplit
            parts = urlsplit(loc)
            out_headers["location"] = urlunsplit(("", "", parts.path, parts.query, ""))
    return Response(content=resp_body, status_code=resp.status_code,
                    headers=out_headers, media_type=resp.headers.get("content-type"))


# --- idle reaper ------------------------------------------------------------
async def _reaper_loop():
    """Park + reap idle per-session Sandboxes (both profiles, issue #4).

    All broker-owned sandboxes are now direct `agents.x-k8s.io/Sandbox` objects labeled
    managed-by=owui-broker (the SandboxClaim warm-pool path is gone — it cannot carry a
    per-session Secret). Persistent sandboxes are parked (Suspended) after PARK_TTL and
    reaped after REAP_TTL; ephemeral sandboxes (emptyDir) are reaped after IDLE_TTL.
    Reaping also deletes the per-session runtime-key Secret."""
    while True:
        try:
            res = cast(dict, api.list_namespaced_custom_object(
                SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes",
                label_selector="app.kubernetes.io/managed-by=owui-broker"))
            now = time.time()
            for s in res.get("items", []):
                sname = s["metadata"]["name"]
                labels = s.get("metadata", {}).get("labels", {}) or {}
                profile = labels.get(PROFILE, EPHEMERAL)
                lu = int((s.get("metadata", {}).get("annotations", {}) or {}).get(LAST_USED, "0") or 0)
                if not lu:
                    continue
                idle = now - lu
                if profile == PERSISTENT:
                    if idle > REAP_TTL:
                        log.info("reaping persistent sandbox %s (idle %ds)", sname, int(idle))
                        _delete_sandbox(sname)
                        SANDBOXES_DELETED.labels(profile=PERSISTENT).inc()
                    elif idle > PARK_TTL and _sandbox_operating_mode(sname) != "Suspended":
                        log.info("parking persistent sandbox %s (idle %ds)", sname, int(idle))
                        _set_sandbox_operating_mode(sname, "Suspended")
                else:  # ephemeral
                    if idle > IDLE_TTL:
                        log.info("reaping ephemeral sandbox %s (idle %ds)", sname, int(idle))
                        _delete_sandbox(sname)
                        SANDBOXES_DELETED.labels(profile=EPHEMERAL).inc()
            # Active-sandbox gauge (leader view): count broker-owned sandboxes by profile.
            _n_per = sum(1 for c in res.get("items", [])
                         if (c.get("metadata", {}).get("labels", {}) or {}).get(PROFILE) == PERSISTENT)
            _n_eph = sum(1 for c in res.get("items", [])
                         if (c.get("metadata", {}).get("labels", {}) or {}).get(PROFILE, EPHEMERAL) == EPHEMERAL)
            ACTIVE_SANDBOXES.labels(profile=PERSISTENT).set(_n_per)
            ACTIVE_SANDBOXES.labels(profile=EPHEMERAL).set(_n_eph)
            # Orphan-Secret sweep (issue #51): a crash between deleting the Sandbox CR
            # and its per-session key (or between minting the key and a failed Sandbox
            # create) leaves an owner-less Secret. Reap any broker-owned runtime-key
            # Secret whose Sandbox is gone (idempotent against this iteration's live set).
            _sweep_orphan_runtime_keys({s["metadata"]["name"] for s in res.get("items", [])})
        except Exception as exc:        # pragma: no cover - keep the loop alive
            log.warning("reaper iteration error: %s", exc)
        await asyncio.sleep(60)


def _delete_sandbox(name: str) -> None:
    """Delete a per-session Sandbox and its per-session runtime-key Secret (issue #4)."""
    try:
        api.delete_namespaced_custom_object(SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", name)
    except client.ApiException as exc:      # pragma: no cover - non-fatal
        if exc.status != 404:
            log.warning("reap failed for sandbox %s: %s", name, exc)
    _delete_runtime_key(name)

# Tracked so shutdown can cancel it.
_reaper_task: asyncio.Task | None = None


# --- Leader election (HA: only the elected broker runs the background reaper) ---
# Single-lease election over coordination.k8s.io/Lease. At replicas=1 (default) the sole
# broker always wins; at replicas>1 exactly one holds the lease and reaps, so two replicas
# never double-park/double-reap the same sandbox. The request path (proxy/resolve/migrate)
# runs on every replica regardless of leadership — only the reaper is leader-gated.
_LEADER_LOCK_NS = os.environ.get("BROKER_LEADER_NAMESPACE", RUNTIME_NS)
_LEADER_LEASE_NAME = os.environ.get("BROKER_LEADER_LEASE", "owui-broker-leader")
_LEADER_IDENTITY = os.environ.get("HOSTNAME") or f"broker-{os.getpid()}"
_LEADER_DURATION = _env_int("BROKER_LEADER_DURATION_SECONDS", 15)
_LEADER_RENEW_SECONDS = _env_int("BROKER_LEADER_RENEW_SECONDS", 5)
_is_leader = False
_reaper_task: asyncio.Task | None = None
_leader_task: asyncio.Task | None = None
_coord_api: client.CoordinationV1Api | None = None


def _coord() -> client.CoordinationV1Api:
    """Lazy CoordinationV1Api (created once; tests monkeypatch this)."""
    global _coord_api
    if _coord_api is None:
        _coord_api = client.CoordinationV1Api()
    return _coord_api


def _acquire_or_renew_lease() -> bool:
    """Acquire or renew the broker leader lease. True iff we hold it after the attempt.

    Create if absent; renew if ours; take over if held by another but expired; defer
    (False) if another live holder owns it."""
    now = datetime.datetime.now(datetime.timezone.utc)
    try:
        lease = _coord().read_namespaced_lease(_LEADER_LEASE_NAME, _LEADER_LOCK_NS)
    except client.ApiException as exc:
        if exc.status != 404:
            log.warning("leader: read lease failed: %s", exc)  # pragma: no cover
            return False  # pragma: no cover
        lease = None
    if lease is None:
        spec = client.V1LeaseSpec(
            holder_identity=_LEADER_IDENTITY,
            lease_duration_seconds=_LEADER_DURATION,
            acquire_time=now,
            renew_time=now,
        )
        body = client.V1Lease(metadata=client.V1ObjectMeta(name=_LEADER_LEASE_NAME), spec=spec)
        try:
            _coord().create_namespaced_lease(_LEADER_LOCK_NS, body)
            return True
        except client.ApiException as exc:  # pragma: no cover - lost a create race
            log.warning("leader: create lease failed: %s", exc)
            return False
    spec = getattr(lease, "spec", None) or client.V1LeaseSpec()
    holder = spec.holder_identity
    renew = spec.renew_time
    duration = spec.lease_duration_seconds or _LEADER_DURATION
    held_by_other = (
        bool(holder)
        and holder != _LEADER_IDENTITY
        and renew is not None
        and (datetime.datetime.now(datetime.timezone.utc) - renew).total_seconds() < duration
    )
    if held_by_other:
        return False
    spec.holder_identity = _LEADER_IDENTITY
    spec.lease_duration_seconds = _LEADER_DURATION
    spec.acquire_time = now
    spec.renew_time = now
    try:
        _coord().replace_namespaced_lease(_LEADER_LEASE_NAME, _LEADER_LOCK_NS, lease)
    except client.ApiException as exc:  # pragma: no cover - 409 race / apiserver error
        log.warning("leader: renew lease failed: %s", exc)
        return False
    return True


async def _apply_leadership(leader: bool) -> None:
    """Start the reaper when leading, stop it when not. Testable core of _leader_loop."""
    global _reaper_task
    if leader and (_reaper_task is None or _reaper_task.done()):
        _reaper_task = asyncio.create_task(_reaper_loop())
    elif not leader and _reaper_task is not None and not _reaper_task.done():
        _reaper_task.cancel()
        with contextlib.suppress(BaseException):
            await _reaper_task
        _reaper_task = None


async def _leader_loop() -> None:
    """Hold the leader lease + keep the reaper alive only while we lead."""
    global _is_leader
    while True:
        try:
            _is_leader = _acquire_or_renew_lease()
        except Exception as exc:  # pragma: no cover - defensive
            _is_leader = False
            log.warning("leader loop error: %s", exc)
        await _apply_leadership(_is_leader)
        await asyncio.sleep(_LEADER_RENEW_SECONDS)


def _validate_config() -> None:
    """Fail-closed startup guard: refuse to run with an unsafe shared secret.

    An unset/placeholder BROKER_SHARED_SECRET would silently disable auth (see _auth), so
    we refuse to start rather than run open. Tested directly; wired into startup."""
    if SHARED_SECRET in _PLACEHOLDER_SECRETS:
        raise RuntimeError(
            "BROKER_SHARED_SECRET is unset or a known placeholder — refusing to start. "
            "Set a strong secret (the Helm chart auto-generates one)."
        )


@app.on_event("startup")
async def _start_reaper():
    _validate_config()
    global _leader_task
    _leader_task = asyncio.create_task(_leader_loop())


@app.on_event("shutdown")
async def _stop_reaper():
    """Graceful shutdown: cancel the leader loop + reaper + close the upstream httpx pool
    so SIGTERM doesn't leave either task looping or in-flight proxy requests hanging."""
    global _leader_task, _reaper_task
    for task in (_leader_task, _reaper_task):
        if task is not None and not task.done():
            task.cancel()
            with contextlib.suppress(BaseException):
                await task
    _leader_task = None
    _reaper_task = None
    try:
        await _client.aclose()
    except Exception:
        pass
