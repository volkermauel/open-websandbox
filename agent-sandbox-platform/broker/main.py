"""code-standard broker — the Open WebUI front door (Phase 3, ephemeral profile).

Per request:
  1. authenticate (shared Bearer + X-User-Id + X-Session-Id)
  2. resolve the session to a sandbox via a deterministic SandboxClaim
     (get-or-create from the warm pool), read the claimed sandbox's pod IP
  3. reverse-proxy to the sandbox-router injecting X-Sandbox-Id /
     X-Sandbox-Namespace / X-Sandbox-Pod-IP (the router's priority-1 resolution)

Stateless: the claim name is derived purely from the (user, session) headers, so a
broker restart re-attaches to existing claims. A background reaper deletes claims
idle longer than BROKER_IDLE_TTL_SECONDS (so ended sessions don't leave pods behind).

Persistent profile (per-user claim + volumeClaimTemplates PVC) is a follow-up.
"""
import asyncio
import hashlib
import hmac
import logging
import os
import time
from typing import Optional, cast

import httpx
from fastapi import FastAPI, HTTPException, Request, Response, Security
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


IDLE_TTL = _env_int("BROKER_IDLE_TTL_SECONDS", 1800)            # 30 min
CLAIM_READY_TIMEOUT = _env_int("BROKER_CLAIM_TIMEOUT_SECONDS", 60)
# allow long-running commands (sandbox MAX_TIMEOUT is 600s)
PROXY_TIMEOUT = _env_int("BROKER_PROXY_TIMEOUT_SECONDS", 660)

GROUP, VER = "extensions.agents.x-k8s.io", "v1beta1"
CLAIM_PREFIX = os.environ.get("BROKER_CLAIM_PREFIX", "owui-")
LAST_USED = "broker-last-used"
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
    """Deterministic, DNS-label-safe claim name from the session (stateless recovery)."""
    digest = hashlib.sha256(f"{user_id}|{session_id}".encode()).hexdigest()[:12]
    return f"{CLAIM_PREFIX}{digest}"


def _get_claim(name: str) -> Optional[dict]:
    try:
        return cast(dict, api.get_namespaced_custom_object(GROUP, VER, RUNTIME_NS, "sandboxclaims", name))
    except client.ApiException as exc:
        if exc.status == 404:
            return None
        raise


def _create_claim(name: str) -> Optional[dict]:
    body = {
        "apiVersion": f"{GROUP}/{VER}",
        "kind": "SandboxClaim",
        "metadata": {"name": name, "namespace": RUNTIME_NS, "labels": MANAGED_BY,
                     "annotations": {LAST_USED: str(int(time.time()))}},
        "spec": {"warmPoolRef": {"name": WARMPOOL}},
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
    """Pod IP from the claim's own status (populated by the controller once Ready)."""
    sbx = (claim.get("status", {}).get("sandbox", {}) or {})
    ips = sbx.get("podIPs") or []
    return ips[0] if ips else None


def _touch(name: str) -> None:
    """Stamp last-used on the claim (best-effort; drives idle-reap)."""
    try:
        api.patch_namespaced_custom_object(
            GROUP, VER, RUNTIME_NS, "sandboxclaims", name,
            {"metadata": {"annotations": {LAST_USED: str(int(time.time()))}}})
    except client.ApiException:        # pragma: no cover - non-fatal
        pass


async def resolve_sandbox(user_id: str, session_id: str) -> tuple[str, str]:
    """Return (sandbox_id, pod_ip), creating the claim if needed and waiting for Ready."""
    name = _claim_name(user_id, session_id)
    claim = _get_claim(name) or _create_claim(name)
    if claim is None:
        raise HTTPException(status_code=500, detail=f"claim {name} could not be created")
    deadline = time.time() + CLAIM_READY_TIMEOUT
    while True:
        if _claim_ready(claim):
            sandbox_id = _sandbox_name(claim)
            if sandbox_id:
                pod_ip = _claim_pod_ip(claim)
                if pod_ip:
                    _touch(name)
                    log.info("session user=%s -> claim=%s sandbox=%s pod=%s", user_id[:32], name, sandbox_id, pod_ip)
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

app = FastAPI(title="code-standard broker")
_client = httpx.AsyncClient(timeout=httpx.Timeout(PROXY_TIMEOUT), follow_redirects=False)


@app.get("/healthz")
async def health():
    return {"status": "ok"}


@app.api_route("/{path:path}", methods=["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"])
async def proxy(path: str, request: Request, _=Security(_auth)):
    user = request.headers.get("X-User-Id", "")
    session = request.headers.get("X-Session-Id", "")
    if not user or not session:
        raise HTTPException(status_code=400, detail="X-User-Id and X-Session-Id headers are required")
    sandbox_id, pod_ip = await resolve_sandbox(user, session)

    fwd = {k: v for k, v in request.headers.items() if k.lower() not in HOP}
    fwd.update({"X-Sandbox-Id": sandbox_id, "X-Sandbox-Namespace": RUNTIME_NS, "X-Sandbox-Pod-IP": pod_ip})
    body = await request.body()
    upstream = httpx.Request(request.method, f"{ROUTER_URL}/{path}",
                             headers=fwd, params=request.query_params, content=body)
    resp = await _client.send(upstream, stream=True)
    return Response(content=await resp.aread(), status_code=resp.status_code,
                    headers={k: v for k, v in resp.headers.items() if k.lower() not in HOP},
                    media_type=resp.headers.get("content-type"))


# --- idle reaper ------------------------------------------------------------
async def _reaper_loop():
    while True:
        try:
            res = cast(dict, api.list_namespaced_custom_object(
                GROUP, VER, RUNTIME_NS, "sandboxclaims",
                label_selector="app.kubernetes.io/managed-by=owui-broker"))
            claims = res.get("items", [])
            now = time.time()
            for c in claims:
                lu = int((c.get("metadata", {}).get("annotations", {}) or {}).get(LAST_USED, "0") or 0)
                if lu and now - lu > IDLE_TTL:
                    log.info("reaping idle claim %s (idle %ds)", c["metadata"]["name"], int(now - lu))
                    try:
                        api.delete_namespaced_custom_object(
                            GROUP, VER, RUNTIME_NS, "sandboxclaims", c["metadata"]["name"])
                    except client.ApiException as exc:      # pragma: no cover - non-fatal
                        log.warning("reap failed for %s: %s", c["metadata"]["name"], exc)
        except Exception as exc:        # pragma: no cover - keep the loop alive
            log.warning("reaper iteration error: %s", exc)
        await asyncio.sleep(60)


@app.on_event("startup")
async def _start_reaper():
    asyncio.create_task(_reaper_loop())
