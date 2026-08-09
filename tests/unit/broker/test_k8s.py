# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""k8s CRUD helper tests — driven via the `api`/`core` MagicMock fixtures (issue #4)."""
from __future__ import annotations

import base64
import types

import main  # type: ignore[import-not-found]
import pytest
from conftest import api_exc, make_sandbox

# --- per-session runtime-key Secret (issue #4) ---------------------------------

def test_runtime_key_secret_name_is_namespaced():
    assert main._runtime_key_secret_name("owui-c-abc") == f"{main.RUNTIME_KEY_PREFIX}owui-c-abc"


def test_mint_runtime_key_is_random_and_long():
    a, b = main._mint_runtime_key(), main._mint_runtime_key()
    assert a != b and len(a) >= 32


def test_ensure_runtime_key_creates_when_missing(core):
    core.read_namespaced_secret.side_effect = api_exc(404)
    main._ensure_runtime_key("sbx-1")
    body = core.create_namespaced_secret.call_args.args[-1]
    assert body["metadata"]["name"] == main._runtime_key_secret_name("sbx-1")
    assert "api-key" in body["stringData"]


def test_ensure_runtime_key_noop_when_present(core):
    core.read_namespaced_secret.return_value = types.SimpleNamespace(data={"api-key": "x"})
    main._ensure_runtime_key("sbx-1")
    core.create_namespaced_secret.assert_not_called()


def test_ensure_runtime_key_other_read_raises(core):
    core.read_namespaced_secret.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._ensure_runtime_key("sbx-1")


def test_rotate_runtime_key_patches_existing(core):
    # _write_runtime_key create -> 409 -> patch
    core.create_namespaced_secret.side_effect = api_exc(409)
    main._rotate_runtime_key("sbx-1")
    core.patch_namespaced_secret.assert_called_once()
    assert "api-key" in core.patch_namespaced_secret.call_args.args[-1]["stringData"]


def test_runtime_key_for_decodes_base64(core):
    val = "per-session-secret"
    core.read_namespaced_secret.return_value = types.SimpleNamespace(
        data={"api-key": base64.b64encode(val.encode()).decode()})
    assert main._runtime_key_for("sbx-1") == val


def test_runtime_key_for_404_returns_none(core):
    core.read_namespaced_secret.side_effect = api_exc(404)
    assert main._runtime_key_for("missing") is None


def test_runtime_auth_headers_resolves_bearer(core):
    val = "per-session-secret"
    core.read_namespaced_secret.return_value = types.SimpleNamespace(
        data={"api-key": base64.b64encode(val.encode()).decode()})
    assert main._runtime_auth_headers("sbx-1") == {"Authorization": f"Bearer {val}"}


def test_runtime_auth_headers_empty_when_missing(core):
    core.read_namespaced_secret.side_effect = api_exc(404)
    assert main._runtime_auth_headers("missing") == {}


def test_delete_runtime_key_404_ok(core):
    core.delete_namespaced_secret.side_effect = api_exc(404)
    main._delete_runtime_key("sbx-1")  # no raise


def test_delete_runtime_key_other_is_besteffort(core, caplog):
    # _delete_runtime_key is best-effort (reap path): a non-404 is logged, not raised,
    # so a single key-delete failure never aborts the sandbox reap.
    core.delete_namespaced_secret.side_effect = api_exc(500)
    main._delete_runtime_key("sbx-1")  # no raise
    assert any("reap runtime key" in r.message for r in caplog.records)


# --- _sandbox_operating_mode -----------------------------------------------------
def test_operating_mode_found(api):
    api.get_namespaced_custom_object.return_value = {"spec": {"operatingMode": "Suspended"}}
    assert main._sandbox_operating_mode("sbx") == "Suspended"


def test_operating_mode_404_none(api):
    api.get_namespaced_custom_object.side_effect = api_exc(404)
    assert main._sandbox_operating_mode("sbx") is None


def test_operating_mode_other_raises(api):
    api.get_namespaced_custom_object.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._sandbox_operating_mode("sbx")


# --- _get_sandbox ----------------------------------------------------------------
def test_get_sandbox_found(api):
    api.get_namespaced_custom_object.return_value = {"metadata": {"name": "sbx"}}
    assert main._get_sandbox("sbx") == {"metadata": {"name": "sbx"}}


def test_get_sandbox_404_none(api):
    api.get_namespaced_custom_object.side_effect = api_exc(404)
    assert main._get_sandbox("missing") is None


def test_get_sandbox_other_raises(api):
    api.get_namespaced_custom_object.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._get_sandbox("sbx")


# --- sandbox status readers ------------------------------------------------------
def test_sandbox_ready():
    assert main._sandbox_ready(make_sandbox(ready=True))
    assert not main._sandbox_ready(make_sandbox(ready=False))


def test_sandbox_pod_ip():
    assert main._sandbox_pod_ip(make_sandbox(pod_ip="10.0.0.9")) == "10.0.0.9"
    assert main._sandbox_pod_ip(make_sandbox(pod_ip=None)) is None


# --- _inject_runtime_key_volume --------------------------------------------------
def test_inject_runtime_key_volume_adds_volume_and_mount():
    pod_tmpl = {"spec": {"volumes": [{"name": "workspace", "emptyDir": {}}],
                         "containers": [{"name": "app", "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]}]}}
    main._inject_runtime_key_volume(pod_tmpl, "sbx-1")
    vols = pod_tmpl["spec"]["volumes"]
    rk = next(v for v in vols if v["name"] == "runtime-key")
    assert rk["secret"]["secretName"] == main._runtime_key_secret_name("sbx-1")
    assert rk["secret"]["items"] == [{"key": "api-key", "path": "api-key"}]
    vm = next(m for m in pod_tmpl["spec"]["containers"][0]["volumeMounts"] if m["name"] == "runtime-key")
    assert vm["mountPath"] == "/etc/runtime-key" and vm["readOnly"] is True


def test_inject_runtime_key_volume_idempotent():
    pod_tmpl = {"spec": {"volumes": [], "containers": [{"name": "app", "volumeMounts": []}]}}
    main._inject_runtime_key_volume(pod_tmpl, "sbx-1")
    main._inject_runtime_key_volume(pod_tmpl, "sbx-1")  # second call must not duplicate
    assert sum(1 for v in pod_tmpl["spec"]["volumes"] if v["name"] == "runtime-key") == 1
    assert sum(1 for m in pod_tmpl["spec"]["containers"][0]["volumeMounts"] if m["name"] == "runtime-key") == 1


# --- _create_sandbox (both profiles) -------------------------------------------
_TEMPLATE = {
    "spec": {"podTemplate": {"spec": {
        "volumes": [{"name": "workspace", "emptyDir": {}}],
        "containers": [{"name": "app", "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]}],
    }}}
}


def test_create_sandbox_persistent_points_workspace_at_pvc_and_injects_key(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "chat-sbx"}}
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    out = main._create_sandbox("chat-sbx", "u1", "s1", main.PERSISTENT)
    assert out == {"metadata": {"name": "chat-sbx"}}
    body = api.create_namespaced_custom_object.call_args.args[-1]
    spec = body["spec"]["podTemplate"]["spec"]
    # workspace -> per-chat PVC subPath
    assert spec["volumes"][0]["persistentVolumeClaim"]["claimName"] == "pvc-1"
    assert spec["containers"][0]["volumeMounts"][0]["subPath"].endswith("sub/")
    # per-session runtime-key volume injected
    rk = next(v for v in spec["volumes"] if v["name"] == "runtime-key")
    assert rk["secret"]["secretName"] == main._runtime_key_secret_name("chat-sbx")
    # labels: managed-by + profile + chat
    assert body["metadata"]["labels"][main.PROFILE] == main.PERSISTENT
    assert body["metadata"]["labels"]["broker-chat"] == "true"


def test_create_sandbox_ephemeral_keeps_emptydir_and_injects_key(api):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "eph-sbx"}}
    main._create_sandbox("eph-sbx", "u1", "s1", main.EPHEMERAL)
    body = api.create_namespaced_custom_object.call_args.args[-1]
    spec = body["spec"]["podTemplate"]["spec"]
    # ephemeral keeps the template's emptyDir workspace (no PVC rewrite)
    assert spec["volumes"][0] == {"name": "workspace", "emptyDir": {}}
    # but still gets the per-session key volume
    assert any(v["name"] == "runtime-key" for v in spec["volumes"])
    assert body["metadata"]["labels"][main.PROFILE] == main.EPHEMERAL
    assert "broker-chat" not in body["metadata"]["labels"]


def test_create_sandbox_409_races_to_get(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.side_effect = api_exc(409)
    fetched = {"metadata": {"name": "chat-sbx", "raced": True}}
    monkeypatch.setattr(main, "_get_sandbox", lambda n: fetched)
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    assert main._create_sandbox("chat-sbx", "u1", "s1", main.PERSISTENT) == fetched


def test_create_sandbox_other_status_raises(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.side_effect = api_exc(500)
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    with pytest.raises(main.client.ApiException):
        main._create_sandbox("chat-sbx", "u1", "s1", main.PERSISTENT)


# --- _ensure_user_pvc ------------------------------------------------------------
def test_ensure_user_pvc_exists(core):
    core.read_namespaced_persistent_volume_claim.return_value = {"metadata": {"name": "pvc"}}
    assert main._ensure_user_pvc("u1") == main._user_pvc_name("u1")
    core.create_namespaced_persistent_volume_claim.assert_not_called()


def test_ensure_user_pvc_creates_on_404(core):
    core.read_namespaced_persistent_volume_claim.side_effect = api_exc(404)
    assert main._ensure_user_pvc("u1") == main._user_pvc_name("u1")
    core.create_namespaced_persistent_volume_claim.assert_called_once()


def test_ensure_user_pvc_create_409_ok(core):
    core.read_namespaced_persistent_volume_claim.side_effect = api_exc(404)
    core.create_namespaced_persistent_volume_claim.side_effect = api_exc(409)
    assert main._ensure_user_pvc("u1") == main._user_pvc_name("u1")


def test_ensure_user_pvc_other_read_raises(core):
    core.read_namespaced_persistent_volume_claim.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._ensure_user_pvc("u1")


def test_ensure_user_pvc_create_other_raises(core):
    core.read_namespaced_persistent_volume_claim.side_effect = api_exc(404)
    core.create_namespaced_persistent_volume_claim.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._ensure_user_pvc("u1")


# --- _persistent_volume (mode switch) -------------------------------------------
def test_persistent_volume_per_user_pvc(monkeypatch):
    monkeypatch.setattr(main, "PERSISTENT_MODE", "per-user-pvc")
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-x")
    name, prefix = main._persistent_volume("u1")
    assert name == "pvc-x"
    assert prefix == ""


def test_persistent_volume_shared_subpath(monkeypatch):
    monkeypatch.setattr(main, "PERSISTENT_MODE", "shared-subpath")
    name, prefix = main._persistent_volume("u1")
    assert name == main.SHARED_PVC
    assert prefix == "users/u1/"


# --- direct coverage of best-effort patchers (their ApiException arms are pragma) -
def test_set_sandbox_operating_mode_patches(api):
    main._set_sandbox_operating_mode("sbx-1", "Suspended")
    api.patch_namespaced_custom_object.assert_called_once()


def test_touch_sandbox_patches(api):
    main._touch_sandbox("sbx-1")
    api.patch_namespaced_custom_object.assert_called_once()
