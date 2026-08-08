# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""k8s CRUD helper tests — driven via the `api`/`core` MagicMock fixtures."""
from __future__ import annotations

import main  # type: ignore[import-not-found]
import pytest
from conftest import api_exc, make_claim, make_sandbox


# --- _get_claim ------------------------------------------------------------------
def test_get_claim_found(api):
    api.get_namespaced_custom_object.return_value = {"metadata": {"name": "c1"}}
    assert main._get_claim("c1") == {"metadata": {"name": "c1"}}


def test_get_claim_404_returns_none(api):
    api.get_namespaced_custom_object.side_effect = api_exc(404)
    assert main._get_claim("missing") is None


def test_get_claim_other_status_raises(api):
    api.get_namespaced_custom_object.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._get_claim("c1")


# --- _create_claim ---------------------------------------------------------------
def test_create_claim_ephemeral_has_no_vct(api):
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "c1"}}
    out = main._create_claim("c1", main.EPHEMERAL)
    assert out == {"metadata": {"name": "c1"}}
    body = api.create_namespaced_custom_object.call_args.args[-1]
    assert "volumeClaimTemplates" not in body["spec"]
    assert "lifecycle" not in body["spec"]


def test_create_claim_persistent_builds_vct(api):
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "c1"}}
    main._create_claim("c1", main.PERSISTENT)
    body = api.create_namespaced_custom_object.call_args.args[-1]
    assert body["spec"]["volumeClaimTemplates"][0]["metadata"]["name"] == "workspace"
    assert body["spec"]["lifecycle"]["shutdownPolicy"] == "Retain"


def test_create_claim_409_races_to_get(api, monkeypatch):
    api.create_namespaced_custom_object.side_effect = api_exc(409)
    fetched = {"metadata": {"name": "c1", "raced": True}}
    monkeypatch.setattr(main, "_get_claim", lambda name: fetched)
    assert main._create_claim("c1", main.EPHEMERAL) == fetched


# --- claim status readers --------------------------------------------------------
def test_claim_ready():
    assert main._claim_ready(make_claim(ready=True))
    assert not main._claim_ready(make_claim(ready=False))


def test_sandbox_name_from_claim():
    assert main._sandbox_name(make_claim(sandbox="sbx-1")) == "sbx-1"
    assert main._sandbox_name(make_claim(sandbox=None)) is None


def test_claim_pod_ip():
    assert main._claim_pod_ip(make_claim(pod_ip="10.0.0.1")) == "10.0.0.1"
    assert main._claim_pod_ip(make_claim(pod_ip=None)) is None


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


# --- _create_chat_sandbox --------------------------------------------------------
_TEMPLATE = {
    "spec": {"podTemplate": {"spec": {
        "volumes": [{"name": "workspace", "emptyDir": {}}],
        "containers": [{"name": "app", "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]}],
    }}}
}


def test_create_chat_sandbox_clones_template_and_points_volume_at_pvc(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.return_value = {"metadata": {"name": "chat-sbx"}}
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    out = main._create_chat_sandbox("chat-sbx", "u1", "s1")
    assert out == {"metadata": {"name": "chat-sbx"}}
    body = api.create_namespaced_custom_object.call_args.args[-1]
    vol = body["spec"]["podTemplate"]["spec"]["volumes"][0]
    assert vol["persistentVolumeClaim"]["claimName"] == "pvc-1"
    vm = body["spec"]["podTemplate"]["spec"]["containers"][0]["volumeMounts"][0]
    assert vm["subPath"].endswith("sub/")


def test_create_chat_sandbox_409_races_to_get(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.side_effect = api_exc(409)
    fetched = {"metadata": {"name": "chat-sbx", "raced": True}}
    monkeypatch.setattr(main, "_get_sandbox", lambda n: fetched)
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    assert main._create_chat_sandbox("chat-sbx", "u1", "s1") == fetched


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


# --- non-409 error propagation ---------------------------------------------------
def test_create_claim_other_status_raises(api):
    api.create_namespaced_custom_object.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._create_claim("c1", main.EPHEMERAL)


def test_create_chat_sandbox_other_status_raises(api, monkeypatch):
    api.get_namespaced_custom_object.return_value = _TEMPLATE
    api.create_namespaced_custom_object.side_effect = api_exc(500)
    monkeypatch.setattr(main, "_ensure_user_pvc", lambda u: "pvc-1")
    monkeypatch.setattr(main, "_subdir_for", lambda s: "sub")
    with pytest.raises(main.client.ApiException):
        main._create_chat_sandbox("chat-sbx", "u1", "s1")


def test_ensure_user_pvc_create_other_raises(core):
    core.read_namespaced_persistent_volume_claim.side_effect = api_exc(404)
    core.create_namespaced_persistent_volume_claim.side_effect = api_exc(500)
    with pytest.raises(main.client.ApiException):
        main._ensure_user_pvc("u1")


# --- direct coverage of best-effort patchers (their ApiException arms are pragma) -
def test_set_sandbox_operating_mode_patches(api):
    main._set_sandbox_operating_mode("sbx-1", "Suspended")
    api.patch_namespaced_custom_object.assert_called_once()


def test_touch_patches_claim(api):
    main._touch("c1")
    api.patch_namespaced_custom_object.assert_called_once()


def test_touch_sandbox_patches(api):
    main._touch_sandbox("sbx-1")
    api.patch_namespaced_custom_object.assert_called_once()
