# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""S3-tiered cold-tier broker tests (issue #52).

A fake in-memory S3 stands in for aioboto3 (not installed in the test env — the broker
soft-imports it, and `_get_s3_client` is the monkeypatch seam). Covers offload (multipart,
keep-latest, expiry metadata), restore (no-op first creation / streams latest / fails the
resume), the periodic-sync sweep (R1), D7 retry+keep-alive, D4 synchronous restore on
resolve, the s3-tiered sandbox create variant, and the fail-closed boot guard.
"""
from __future__ import annotations

import asyncio
import contextlib
from unittest.mock import AsyncMock

import main  # type: ignore[import-not-found]
import pytest
from conftest import make_sandbox
from fastapi import HTTPException


# --- fake in-memory S3 -----------------------------------------------------------
class _FakeBody:
    """async-iterable stand-in for an aiobotocore get_object() body."""

    def __init__(self, data: bytes):
        self._data, self._pos = bytes(data), 0

    async def iter_chunks(self, _chunk_size: int = 8192):
        while self._pos < len(self._data):
            chunk = self._data[self._pos:self._pos + _chunk_size]
            self._pos += len(chunk)
            yield chunk


class FakeS3:
    """A minimal async S3: multipart upload, list/get/delete, all in memory."""

    def __init__(self):
        self.objects: dict[str, dict] = {}
        self._uploads: dict[str, dict] = {}
        self._n = 0

    async def create_multipart_upload(self, *, Bucket, Key, **kw):
        self._n += 1
        self._uploads[str(self._n)] = {"key": Key, "parts": {}, "kw": kw}
        return {"UploadId": str(self._n)}

    async def upload_part(self, *, Bucket, Key, PartNumber, UploadId, Body):
        self._uploads[UploadId]["parts"][PartNumber] = bytes(Body)
        return {"ETag": f"etag-{PartNumber}"}

    async def complete_multipart_upload(self, *, Bucket, Key, UploadId, MultipartUpload):
        up = self._uploads.pop(UploadId)
        data = b"".join(up["parts"][p["PartNumber"]] for p in MultipartUpload["Parts"])
        self.objects[Key] = {"data": data, **up["kw"]}
        return {"ETag": "final"}

    async def abort_multipart_upload(self, *, Bucket, Key, UploadId):
        self._uploads.pop(UploadId, None)

    async def list_objects_v2(self, *, Bucket, Prefix):
        cs = [{"Key": k} for k in sorted(self.objects) if k.startswith(Prefix)]
        return {"Contents": cs} if cs else {}

    async def delete_objects(self, *, Bucket, Delete):
        # Batch DeleteObjects must NOT be used: MinIO requires Content-MD5 on it
        # (botocore omits it). Retention uses per-object delete_object instead.
        # Raising here guards against re-introducing the batch call (regression).
        raise AssertionError("batch delete_objects must not be used; use delete_object")

    async def delete_object(self, *, Bucket, Key):
        # Single-object delete (portable: MinIO rejects batch DeleteObjects without
        # Content-MD5). Mirrors real S3-compatible stores.
        self.objects.pop(Key, None)

    async def get_object(self, *, Bucket, Key):
        return {"Body": _FakeBody(self.objects[Key]["data"])}


@pytest.fixture
def s3_env(monkeypatch):
    """Enable S3-tiered mode (off by default in the shared conftest)."""
    monkeypatch.setattr(main, "S3_ENABLED", True)
    monkeypatch.setattr(main, "S3_TIERED", True)
    monkeypatch.setattr(main, "PERSISTENT_MODE", main.S3_TIERED_MODE)
    monkeypatch.setattr(main, "S3_BUCKET", "test-bucket")
    monkeypatch.setattr(main, "S3_PREFIX", "users")
    monkeypatch.setattr(main, "S3_RETENTION_DAYS", 7)
    monkeypatch.setattr(main, "S3_SIZE_LIMIT", "2Gi")
    monkeypatch.setattr(main, "S3_TMPFS", False)


@pytest.fixture
def fake_s3(monkeypatch, s3_env):
    """Install a FakeS3 as the broker's S3 client (the test seam)."""
    s3 = FakeS3()

    @contextlib.asynccontextmanager
    async def _cm():
        yield s3

    monkeypatch.setattr(main, "_get_s3_client", _cm)
    return s3


def _snapshot_resp(data: bytes, status: int = 200):
    """A fake httpx streaming response for GET /snapshot."""
    resp = AsyncMock()
    resp.status_code = status

    async def _aiter(_n):
        yield data

    resp.aiter_bytes = _aiter
    return resp


# --- object key format (D3) ------------------------------------------------------
def test_object_key_is_versioned_zero_padded():
    key = main._s3_object_key("alice", "sess-1", 1234)
    assert key.startswith(main._s3_prefix("alice", "sess-1"))
    assert key.endswith("workspace-0000001234.tar.zst")  # zero-padded -> lexical == chronological
    assert main._s3_object_key("u", "s", 5) < main._s3_object_key("u", "s", 100)


# --- sandbox create variant (D2/D9) ---------------------------------------------
def test_create_sandbox_s3_tiered_emptydir(api, monkeypatch, s3_env):
    api.get_namespaced_custom_object.return_value = {"spec": {"podTemplate": {"spec": {
        "volumes": [{"name": "workspace", "emptyDir": {"sizeLimit": "4Gi"}}],
        "containers": [{"name": "runtime", "volumeMounts": [{"name": "workspace"}]}],
    }}}}
    monkeypatch.setattr(main, "_ensure_runtime_key", lambda n: "k")
    main._create_sandbox("owui-c-x", "alice", "sess-1", main.PERSISTENT)
    body = api.create_namespaced_custom_object.call_args.args[4]
    vol = next(v for v in body["spec"]["podTemplate"]["spec"]["volumes"] if v["name"] == "workspace")
    assert "persistentVolumeClaim" not in vol        # no PVC (D2)
    assert vol["emptyDir"]["sizeLimit"] == "2Gi"     # size-limited hot tier (D9)
    vm = body["spec"]["podTemplate"]["spec"]["containers"][0]["volumeMounts"][0]
    assert "subPath" not in vm                       # whole workspace is the hot tier
    assert body["metadata"]["labels"].get(main.PERSISTENT_MODE_LABEL) == "s3-tiered"


def test_create_sandbox_s3_tiered_tmpfs(api, monkeypatch, s3_env):
    main.S3_TMPFS = True
    try:
        api.get_namespaced_custom_object.return_value = {"spec": {"podTemplate": {"spec": {
            "volumes": [{"name": "workspace", "emptyDir": {}}],
            "containers": [{"name": "runtime", "volumeMounts": []}],
        }}}}
        monkeypatch.setattr(main, "_ensure_runtime_key", lambda n: "k")
        main._create_sandbox("owui-c-x", "alice", "sess-1", main.PERSISTENT)
        body = api.create_namespaced_custom_object.call_args.args[4]
        vol = next(v for v in body["spec"]["podTemplate"]["spec"]["volumes"] if v["name"] == "workspace")
        assert vol["emptyDir"]["medium"] == "Memory"   # tmpfs option (D2)
        assert vol["emptyDir"]["sizeLimit"] == "2Gi"
    finally:
        main.S3_TMPFS = False


# --- offload (D1/D3) + retention (R2/D5) ----------------------------------------
async def test_offload_writes_object_keep_latest_with_expiry(fake_s3, monkeypatch, httpx_client):
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    ts = iter([1700000000, 1700000005])
    monkeypatch.setattr(main, "_now_ts", lambda: next(ts))
    httpx_client.get.return_value = _snapshot_resp(b"ws-bytes-v1")

    k1 = await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1", final=True)
    httpx_client.get.return_value = _snapshot_resp(b"ws-bytes-v2")
    k2 = await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1", final=True)

    assert k1 != k2
    keys = list(fake_s3.objects)                      # keep-latest: exactly one object (R2)
    assert len(keys) == 1 and keys[0] == k2
    assert fake_s3.objects[k2]["data"] == b"ws-bytes-v2"
    assert fake_s3.objects[k2]["ServerSideEncryption"] == "AES256"   # SSE-S3 (D9)
    assert fake_s3.objects[k2]["Expires"] is not None               # expiry metadata (R2/D5)


async def test_offload_omits_sse_when_disabled(fake_s3, monkeypatch, httpx_client):
    """SSE-S3 is conditional: with S3_SSE="" the broker omits the header (dev MinIO has no KMS)."""
    monkeypatch.setattr(main, "S3_SSE", "")
    monkeypatch.setattr(main, "_now_ts", lambda: 1700000000)
    httpx_client.get.return_value = _snapshot_resp(b"no-sse")
    key = await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1", final=True)
    assert "ServerSideEncryption" not in fake_s3.objects[key]      # header omitted entirely


async def test_offload_failure_raises(fake_s3, monkeypatch, httpx_client):
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    monkeypatch.setattr(main, "_now_ts", lambda: 1700000000)
    httpx_client.get.return_value = _snapshot_resp(b"", status=413)   # workspace too large (D9)
    with pytest.raises(RuntimeError):
        await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")
    assert fake_s3.objects == {}                      # nothing written


async def test_offload_keeps_old_object_when_upload_fails(fake_s3, monkeypatch, httpx_client):
    """Offload uploads the NEW object before deleting the OLD one, so a mid-offload failure
    (broker crash / upload error) never empties the prefix — the previous snapshot stays
    restorable. Guards the upload-new-then-delete-old ordering (D7, #56)."""
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    ts = iter([1700000000, 1700000005])
    monkeypatch.setattr(main, "_now_ts", lambda: next(ts))
    httpx_client.get.return_value = _snapshot_resp(b"ws-v1")
    old_key = await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1", final=True)
    assert old_key in fake_s3.objects                       # prior snapshot seeded

    # second offload whose S3 upload blows up after the snapshot GET succeeds
    async def _boom(*a, **kw):
        raise RuntimeError("upload exploded")
    monkeypatch.setattr(main, "_s3_multipart_stream", _boom)
    httpx_client.get.return_value = _snapshot_resp(b"ws-v2")
    with pytest.raises(RuntimeError):
        await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1", final=True)

    assert old_key in fake_s3.objects                       # OLD survived: delete ran AFTER upload
    assert list(fake_s3.objects) == [old_key]               # prefix not empty -> restore still finds it


async def test_offload_uses_multipart_for_large_snapshot(fake_s3, monkeypatch, httpx_client):
    """A snapshot larger than the part size is split into multiple S3 parts (D3)."""
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    monkeypatch.setattr(main, "S3_PART_SIZE", 4)       # 10 bytes -> 3 parts (4,4,2)
    monkeypatch.setattr(main, "_now_ts", lambda: 1700000000)
    httpx_client.get.return_value = _snapshot_resp(b"0123456789")
    key = await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")
    # the fake S3 reassembles parts in order -> exact payload round-trips
    assert fake_s3.objects[key]["data"] == b"0123456789"


# --- restore (D4 / D7) -----------------------------------------------------------
async def test_restore_noop_first_creation(fake_s3, httpx_client):
    assert await main._restore_from_s3("owui-c-1", "10.0.0.1", "alice", "sess-1") is None
    httpx_client.put.assert_not_called()


async def test_restore_streams_latest(fake_s3, monkeypatch, httpx_client):
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    monkeypatch.setattr(main, "_now_ts", lambda: 1700000000)
    httpx_client.get.return_value = _snapshot_resp(b"payload")
    await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")
    put_resp = AsyncMock()
    put_resp.status_code = 200
    httpx_client.put.return_value = put_resp

    key = await main._restore_from_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")
    assert key is not None and key.endswith(".tar.zst")
    httpx_client.put.assert_awaited_once()


async def test_restore_failure_raises_not_empty(fake_s3, monkeypatch, httpx_client):
    monkeypatch.setattr(main, "PROXY_TIMEOUT", 30)
    monkeypatch.setattr(main, "_now_ts", lambda: 1700000000)
    httpx_client.get.return_value = _snapshot_resp(b"payload")
    await main._offload_to_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")
    put_resp = AsyncMock()
    put_resp.status_code = 500                        # PUT /restore failed -> fail resume (D7)
    httpx_client.put.return_value = put_resp
    with pytest.raises(RuntimeError):
        await main._restore_from_s3("owui-c-1", "10.0.0.1", "alice", "sess-1")


# --- D7 retry + keep-alive -------------------------------------------------------
async def test_offload_retry_then_keep_alive(monkeypatch, s3_env):
    """Persistent offload failure -> retry exhausts -> returns False (reaper keeps pod alive)."""
    calls = {"n": 0}

    async def _always_fail(*a, **k):
        calls["n"] += 1
        raise RuntimeError("boom")

    monkeypatch.setattr(main, "_offload_to_s3", _always_fail)
    monkeypatch.setattr(main, "S3_OFFLOAD_MAX_ATTEMPTS", 3)
    monkeypatch.setattr(main, "S3_OFFLOAD_BACKOFF_SECONDS", 0)
    monkeypatch.setattr(main.asyncio, "sleep", AsyncMock())
    assert await main._offload_to_s3_with_retry("s", "ip", "u", "s") is False
    assert calls["n"] == 3


async def test_offload_retry_succeeds_eventually(monkeypatch, s3_env):
    """Offload fails then succeeds on retry -> returns True (D7 backoff pays off)."""
    seq = iter([RuntimeError("transient"), None])

    async def _flaky(*a, **k):
        exc = next(seq)
        if exc:
            raise exc

    monkeypatch.setattr(main, "_offload_to_s3", _flaky)
    monkeypatch.setattr(main, "S3_OFFLOAD_MAX_ATTEMPTS", 3)
    monkeypatch.setattr(main, "S3_OFFLOAD_BACKOFF_SECONDS", 0)
    monkeypatch.setattr(main.asyncio, "sleep", AsyncMock())
    assert await main._offload_to_s3_with_retry("s", "ip", "u", "s") is True


# --- periodic sync (R1) ----------------------------------------------------------
async def test_periodic_sync_once_offloads_running(fake_s3, api, monkeypatch):
    running = make_sandbox(name="owui-c-1", profile="persistent", operating_mode="Running",
                           pod_ip="10.0.0.1", chat=True)
    running["metadata"]["labels"][main.PERSISTENT_MODE_LABEL] = main.S3_TIERED_MODE
    running["metadata"]["annotations"] = {"broker-user": "alice", "broker-session": "sess-1"}
    pvc = make_sandbox(name="owui-c-pvc", profile="persistent", operating_mode="Running",
                       pod_ip="10.0.0.2", chat=True)               # not s3-tiered -> skipped
    suspended = make_sandbox(name="owui-c-susp", profile="persistent", operating_mode="Suspended",
                             pod_ip=None, chat=True)
    suspended["metadata"]["labels"][main.PERSISTENT_MODE_LABEL] = main.S3_TIERED_MODE  # Suspended -> skipped
    api.list_namespaced_custom_object.return_value = {"items": [running, pvc, suspended]}

    async def _fake_offload(sname, ip, uid, sid, *, final=True):
        assert final is False                          # periodic, not the final reap snapshot
        return "k"

    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_offload_to_s3", _fake_offload)
    assert await main._periodic_sync_once() == 1       # only the running s3-tiered sandbox


async def test_apply_leadership_lifecycle_periodic_task(monkeypatch, s3_env):
    """The leader starts the periodic-sync task when S3 is enabled, and stops it on loss."""
    main._reaper_task = None
    main._periodic_task = None
    monkeypatch.setattr(main, "_reaper_loop", AsyncMock())

    async def _block(*_a, **_k):
        await asyncio.sleep(10000)   # stay alive until cancelled

    monkeypatch.setattr(main, "_periodic_sync_loop", _block)
    try:
        await main._apply_leadership(True)            # leader + S3 -> create the periodic task
        assert main._periodic_task is not None and not main._periodic_task.done()
        await main._apply_leadership(False)           # leadership lost -> cancel + clear
        assert main._periodic_task is None
    finally:
        for t in (main._reaper_task, main._periodic_task):
            if t is not None and not t.done():
                t.cancel()
                with contextlib.suppress(BaseException):
                    await t
        main._reaper_task = None
        main._periodic_task = None


# --- D4 synchronous restore on resolve ------------------------------------------
async def test_resolve_restore_blocks_readiness_on_failure(monkeypatch, s3_env):
    """Restore failure on resume -> resolve raises HTTP 502 (D4/D7: never start empty)."""
    ready = make_sandbox(name="owui-c-1", profile="persistent", pod_ip="10.0.0.9")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: ready)        # existing -> resume
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: ready)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)

    async def _boom(*a, **k):
        raise RuntimeError("s3 down")

    monkeypatch.setattr(main, "_restore_from_s3", _boom)
    with pytest.raises(HTTPException) as exc:
        await main.resolve_sandbox("alice", "sess-1", main.PERSISTENT)
    assert exc.value.status_code == 502


async def test_resolve_restore_happy_path(monkeypatch, s3_env):
    ready = make_sandbox(name="owui-c-1", profile="persistent", pod_ip="10.0.0.9")
    monkeypatch.setattr(main, "_get_sandbox", lambda n: ready)
    monkeypatch.setattr(main, "_sandbox_operating_mode", lambda n: "Running")
    monkeypatch.setattr(main, "_watch_until_ready", lambda *a, **k: ready)
    monkeypatch.setattr(main, "_touch_sandbox", lambda n: None)
    restore = AsyncMock(return_value="users/x/chats/y/workspace-0000000001.tar.zst")
    monkeypatch.setattr(main, "_restore_from_s3", restore)
    name, ip = await main.resolve_sandbox("alice", "sess-1", main.PERSISTENT)
    assert (name, ip) == (main._chat_sandbox_name("alice", "sess-1"), "10.0.0.9")
    restore.assert_awaited_once()


# --- reaper keep-alive (D7) ------------------------------------------------------
async def _reaper_one_tick(monkeypatch):
    import asyncio as _aio

    async def _break(*_a, **_k):  # CancelledError after the first full reaper iteration
        raise _aio.CancelledError()

    monkeypatch.setattr(main.asyncio, "sleep", _break)
    task = _aio.create_task(main._reaper_loop())
    try:
        await task
    except BaseException:
        pass


async def test_reaper_keeps_sandbox_alive_on_offload_failure(api, monkeypatch, s3_env):
    """s3-tiered sandbox whose offload fails is NOT deleted (D7)."""
    old = main.IDLE_TTL + 5
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=-old,
                       operating_mode="Running", pod_ip="10.0.0.1", chat=True)
    sbx["metadata"]["labels"][main.PERSISTENT_MODE_LABEL] = main.S3_TIERED_MODE
    sbx["metadata"]["annotations"].update({"broker-user": "alice", "broker-session": "sess-1"})
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    monkeypatch.setattr(main, "_offload_to_s3_with_retry", AsyncMock(return_value=False))
    deleted: list[str] = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _reaper_one_tick(monkeypatch)
    assert deleted == []                              # kept alive: offload failed


async def test_reaper_reaps_after_successful_offload(api, monkeypatch, s3_env):
    """s3-tiered sandbox whose offload succeeds IS reaped (D7 happy path)."""
    old = main.IDLE_TTL + 5
    sbx = make_sandbox(name="owui-c-1", profile="persistent", last_used=-old,
                       operating_mode="Running", pod_ip="10.0.0.1", chat=True)
    sbx["metadata"]["labels"][main.PERSISTENT_MODE_LABEL] = main.S3_TIERED_MODE
    sbx["metadata"]["annotations"].update({"broker-user": "alice", "broker-session": "sess-1"})
    api.list_namespaced_custom_object.return_value = {"items": [sbx]}
    monkeypatch.setattr(main, "_offload_to_s3_with_retry", AsyncMock(return_value=True))
    deleted: list[str] = []
    monkeypatch.setattr(main, "_delete_sandbox", lambda n: deleted.append(n))
    await _reaper_one_tick(monkeypatch)
    assert deleted == ["owui-c-1"]                    # reaped after a successful offload


# --- fail-closed boot guard ------------------------------------------------------
def test_validate_config_rejects_s3tiered_without_s3(monkeypatch):
    monkeypatch.setattr(main, "S3_TIERED", True)
    monkeypatch.setattr(main, "S3_ENABLED", False)
    with pytest.raises(RuntimeError):
        main._validate_config()


def test_validate_config_rejects_s3_enabled_without_endpoint(monkeypatch):
    monkeypatch.setattr(main, "S3_TIERED", False)
    monkeypatch.setattr(main, "S3_ENABLED", True)
    monkeypatch.setattr(main, "S3_BUCKET", "")
    monkeypatch.setattr(main, "S3_ENDPOINT", "")
    with pytest.raises(RuntimeError):
        main._validate_config()
