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
import contextlib
import hashlib
import hmac
import json
import logging
import os
import time
from typing import Optional, cast

import httpx
import websockets
from fastapi import FastAPI, HTTPException, Request, Response, Security, WebSocket, WebSocketDisconnect
from openapi_spec import OPENAPI
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from kubernetes import client, config

# --- config -----------------------------------------------------------------
SHARED_SECRET = os.environ.get("BROKER_SHARED_SECRET", "")
WARMPOOL = os.environ.get("BROKER_WARMPOOL", "code-standard-warmpool")
RUNTIME_NS = os.environ.get("BROKER_RUNTIME_NS", "agent-sandbox-runtime")
ROUTER_URL = os.environ.get("BROKER_ROUTER_URL", "http://sandbox-router-svc.agent-sandbox-system:8080")


def _env_int(name: str, default: int) -> int:
    """Parse an int env var, falling back to default on missing/bad input."""
    try:
        return int(os.environ.get(name, str(default)))
    except (TypeError, ValueError):
        return default


IDLE_TTL = _env_int("BROKER_IDLE_TTL_SECONDS", 1800)                 # ephemeral reap: 30 min
PARK_TTL = _env_int("BROKER_PARK_IDLE_SECONDS", 1800)                # persistent suspend: 30 min
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
    DEFAULT_PROFILE = PERSISTENT
MANAGED_BY = {"app.kubernetes.io/managed-by": "owui-broker"}

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("broker")

# --- k8s --------------------------------------------------------------------
try:
    config.load_incluster_config()
except Exception:                      # pragma: no cover - local dev fallback
    config.load_kube_config()
api = client.CustomObjectsApi()

bearer = HTTPBearer(auto_error=False)


def _auth(credentials: Optional[HTTPAuthorizationCredentials] = Security(bearer)) -> None:
    """Validate the shared Bearer secret (constant-time). Unset secret = disabled (dev)."""
    if not SHARED_SECRET:
        return
    if not credentials or not hmac.compare_digest(credentials.credentials.encode(), SHARED_SECRET.encode()):
        raise HTTPException(status_code=401, detail="invalid bearer token")


# --- session -> sandbox -----------------------------------------------------
def _claim_name(user_id: str, session_id: str) -> str:
    """Deterministic, DNS-label-safe ephemeral claim name (one per session)."""
    digest = hashlib.sha256(f"{user_id}|{session_id}".encode()).hexdigest()[:12]
    return f"{CLAIM_PREFIX}{digest}"


def _persistent_claim_name(user_id: str) -> str:
    """Deterministic per-USER persistent claim name (resumed by any session)."""
    return f"{PERSISTENT_PREFIX}{hashlib.sha256(user_id.encode()).hexdigest()[:12]}"


def _get_claim(name: str) -> Optional[dict]:
    try:
        return cast(dict, api.get_namespaced_custom_object(GROUP, VER, RUNTIME_NS, "sandboxclaims", name))
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise


def _create_claim(name: str, profile: str) -> Optional[dict]:
    spec: dict = {"warmPoolRef": {"name": WARMPOOL}}
    if profile == PERSISTENT:
        # Forces a cold start (warm pool pods have no PVC). The controller merges this
        # `workspace` VCT into the pod, replacing the template's same-named emptyDir, so
        # /workspace is backed by a per-user cephfs PVC. lifecycle.Retain keeps the
        # Sandbox object (and its PVC) when the pod is shut down on expiry.
        spec["volumeClaimTemplates"] = [{
            "metadata": {"name": "workspace"},
            "spec": {"accessModes": ["ReadWriteMany"], "storageClassName": PERSISTENT_SC,
                     "resources": {"requests": {"storage": PERSISTENT_SIZE}}},
        }]
        spec["lifecycle"] = {"shutdownPolicy": "Retain"}
    body = {
        "apiVersion": f"{GROUP}/{VER}",
        "kind": "SandboxClaim",
        "metadata": {"name": name, "namespace": RUNTIME_NS,
                     "labels": {**MANAGED_BY, PROFILE: profile},
                     "annotations": {LAST_USED: str(int(time.time()))}},
        "spec": spec,
    }
    try:
        return cast(dict, api.create_namespaced_custom_object(GROUP, VER, RUNTIME_NS, "sandboxclaims", body))
    except client.ApiException as exc:
        if exc.status == 409:           # raced with another broker replica; fetch
            return _get_claim(name)
        raise


def _claim_ready(claim: dict) -> bool:
    return any(c.get("type") == "Ready" and c.get("status") == "True"
               for c in claim.get("status", {}).get("conditions", []))


def _sandbox_name(claim: dict) -> Optional[str]:
    return (claim.get("status", {}).get("sandbox", {}) or {}).get("name")


def _claim_pod_ip(claim: dict) -> Optional[str]:
    """Pod IP from the claim's own status (populated by the controller once the pod is up)."""
    sbx = (claim.get("status", {}).get("sandbox", {}) or {})
    ips = sbx.get("podIPs") or []
    return ips[0] if ips else None


def _sandbox_operating_mode(sandbox_name: str) -> Optional[str]:
    try:
        sbx = cast(dict, api.get_namespaced_custom_object(SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", sandbox_name))
        return sbx.get("spec", {}).get("operatingMode")
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise


def _set_sandbox_operating_mode(sandbox_name: str, mode: str) -> None:
    """Park (Suspended) or resume (Running) a persistent sandbox. operatingMode is a
    Sandbox-only field (not in the SandboxClaim blueprint), so the claim controller
    does not fight this patch."""
    try:
        api.patch_namespaced_custom_object(
            SANDBOX_GROUP, VER, RUNTIME_NS, "sandboxes", sandbox_name,
            {"spec": {"operatingMode": mode}})
        log.info("sandbox %s operatingMode -> %s", sandbox_name, mode)
    except client.ApiException as exc:        # pragma: no cover - non-fatal
        log.warning("operatingMode patch %s=%s failed: %s", sandbox_name, mode, exc)


def _touch(name: str) -> None:
    """Stamp last-used on the claim (best-effort; drives idle park/reap)."""
    try:
        api.patch_namespaced_custom_object(
            GROUP, VER, RUNTIME_NS, "sandboxclaims", name,
            {"metadata": {"annotations": {LAST_USED: str(int(time.time()))}}})
    except client.ApiException:        # pragma: no cover - non-fatal
        pass


async def resolve_sandbox(user_id: str, session_id: str, profile: str) -> tuple[str, str]:
    """Return (sandbox_id, pod_ip): get-or-create the claim, resuming a parked
    persistent sandbox if necessary, then wait for the pod IP to appear."""
    name = _persistent_claim_name(user_id) if profile == PERSISTENT else _claim_name(user_id, session_id)
    claim = _get_claim(name) or _create_claim(name, profile)
    if claim is None:
        raise HTTPException(status_code=500, detail=f"claim {name} could not be created")
    deadline = time.time() + CLAIM_READY_TIMEOUT
    while True:
        sandbox_id = _sandbox_name(claim)
        if profile == PERSISTENT and sandbox_id and _sandbox_operating_mode(sandbox_id) == "Suspended":
            # Resume a parked sandbox: flip operatingMode; the pod (and pod IP) return shortly.
            _set_sandbox_operating_mode(sandbox_id, "Running")
        if _claim_ready(claim):
            sandbox_id = _sandbox_name(claim)
            if sandbox_id:
                pod_ip = _claim_pod_ip(claim)
                if pod_ip:
                    _touch(name)
                    log.info("session user=%s profile=%s -> claim=%s sandbox=%s pod=%s",
                             user_id[:32], profile, name, sandbox_id, pod_ip)
                    return sandbox_id, pod_ip
        if time.time() > deadline:
            raise HTTPException(status_code=504, detail=f"sandbox claim {name} not ready in {CLAIM_READY_TIMEOUT}s")
        await asyncio.sleep(1)
        claim = _get_claim(name)
        if claim is None:
            raise HTTPException(status_code=500, detail=f"claim {name} vanished during resolve")


# --- reverse proxy ----------------------------------------------------------
HOP = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te",
       "trailers", "transfer-encoding", "upgrade", "host", "content-length", "authorization"}


app = FastAPI(title="code-standard broker", docs_url=None, redoc_url=None, openapi_url=None)
_client = httpx.AsyncClient(timeout=httpx.Timeout(PROXY_TIMEOUT), follow_redirects=False)


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
            headers={"Authorization": f"Bearer {SHARED_SECRET}", "X-Session-Id": session_id},
        )

    # Connect directly to the runtime pod (the broker already resolved its IP; the
    # runtime NP allows agent-sandbox-system ingress on 8888). Bypassing the router
    # avoids its WebSocket-upgrade handling, which times out on the opening handshake.
    upstream = f"ws://{pod_ip}:8888/api/terminals/{session_id}"
    log.info("terminal ws user=%s session=%s -> sandbox=%s pod=%s", user[:32], session[:32], sandbox_id, pod_ip)
    try:
        async with websockets.connect(upstream) as up_ws:

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
                except WebSocketDisconnect:
                    pass

            async def _upstream_to_client():
                try:
                    async for msg in up_ws:
                        if isinstance(msg, bytes):
                            await client_ws.send_bytes(msg)
                        else:
                            await client_ws.send_text(msg)
                except Exception:
                    pass

            await asyncio.gather(_client_to_upstream(), _upstream_to_client(), return_exceptions=True)
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
    if profile == PERSISTENT:
        # Isolate each chat under its own folder on the shared per-user PVC.
        fwd["X-Workspace-Subdir"] = _subdir_for(session)
    body = await request.body()
    upstream = httpx.Request(request.method, f"{ROUTER_URL}/{path}",
                             headers=fwd, params=request.query_params, content=body)
    resp = await _client.send(upstream, stream=True)
    resp_body = await resp.aread()
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
    while True:
        try:
            res = cast(dict, api.list_namespaced_custom_object(
                GROUP, VER, RUNTIME_NS, "sandboxclaims",
                label_selector="app.kubernetes.io/managed-by=owui-broker"))
            now = time.time()
            for c in res.get("items", []):
                name = c["metadata"]["name"]
                labels = c.get("metadata", {}).get("labels", {}) or {}
                profile = labels.get(PROFILE, EPHEMERAL)
                lu = int((c.get("metadata", {}).get("annotations", {}) or {}).get(LAST_USED, "0") or 0)
                if not lu:
                    continue
                idle = now - lu
                sandbox = _sandbox_name(c)
                if profile == PERSISTENT:
                    if idle > REAP_TTL:
                        log.info("reaping persistent claim %s (idle %ds)", name, int(idle))
                        _delete_claim(name)
                    elif idle > PARK_TTL and sandbox and _sandbox_operating_mode(sandbox) != "Suspended":
                        log.info("parking persistent claim %s sandbox=%s (idle %ds)", name, sandbox, int(idle))
                        _set_sandbox_operating_mode(sandbox, "Suspended")
                else:  # ephemeral
                    if idle > IDLE_TTL:
                        log.info("reaping ephemeral claim %s (idle %ds)", name, int(idle))
                        _delete_claim(name)
        except Exception as exc:        # pragma: no cover - keep the loop alive
            log.warning("reaper iteration error: %s", exc)
        await asyncio.sleep(60)


def _delete_claim(name: str) -> None:
    try:
        api.delete_namespaced_custom_object(GROUP, VER, RUNTIME_NS, "sandboxclaims", name)
    except client.ApiException as exc:      # pragma: no cover - non-fatal
        log.warning("reap failed for %s: %s", name, exc)


@app.on_event("startup")
async def _start_reaper():
    asyncio.create_task(_reaper_loop())
