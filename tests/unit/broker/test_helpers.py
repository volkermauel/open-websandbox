"""Pure helper + config-normalization tests (no k8s/httpx mocking needed)."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import main  # type: ignore[import-not-found]


# --- _env_int --------------------------------------------------------------------
def test_env_int_valid(monkeypatch):
    monkeypatch.setenv("OS_ENV_INT_TEST", "42")
    assert main._env_int("OS_ENV_INT_TEST", 5) == 42


def test_env_int_invalid(monkeypatch):
    monkeypatch.setenv("OS_ENV_INT_TEST", "not-a-number")
    assert main._env_int("OS_ENV_INT_TEST", 5) == 5


def test_env_int_missing(monkeypatch):
    monkeypatch.delenv("OS_ENV_INT_TEST", raising=False)
    assert main._env_int("OS_ENV_INT_TEST", 7) == 7


# --- deterministic name helpers --------------------------------------------------
def test_claim_name_is_deterministic_and_prefixed():
    a = main._claim_name("user-1", "sess-1")
    b = main._claim_name("user-1", "sess-1")
    assert a == b
    assert a.startswith(main.CLAIM_PREFIX)
    assert a != main._claim_name("user-1", "sess-2")


def test_persistent_claim_name_is_per_user():
    a = main._persistent_claim_name("user-1")
    assert a.startswith(main.PERSISTENT_PREFIX)
    # same user → same name; different user → different name
    assert a == main._persistent_claim_name("user-1")
    assert a != main._persistent_claim_name("user-2")


def test_chat_sandbox_name_is_per_chat():
    a = main._chat_sandbox_name("user-1", "chat-1")
    assert a.startswith(main.CHAT_PREFIX)
    assert a == main._chat_sandbox_name("user-1", "chat-1")
    assert a != main._chat_sandbox_name("user-1", "chat-2")


def test_user_pvc_name():
    n = main._user_pvc_name("user-1")
    assert n.startswith(main.PER_USER_PVC_PREFIX)
    assert n != main._user_pvc_name("user-2")


def test_subdir_for_is_hex():
    s = main._subdir_for("chat-1")
    assert len(s) == 16
    assert all(c in "0123456789abcdef" for c in s)


# --- config normalization (lines 73-74, 80-81) run at import → isolated subprocess
def test_profile_normalization_invalid_falls_back(tmp_path):
    """Invalid BROKER_DEFAULT_PROFILE / BROKER_PERSISTENT_MODE normalise to defaults."""
    repo = Path(main.__file__).resolve().parents[2]  # type: ignore[arg-type]
    code = (
        "import os,sys\n"
        "os.environ['BROKER_DEFAULT_PROFILE']='BOGUS'\n"
        "os.environ['BROKER_PERSISTENT_MODE']='BOGUS'\n"
        "import kubernetes.client as kc,kubernetes.config as kcfg\n"
        "kcfg.load_incluster_config=kcfg.load_kube_config=lambda *a,**k:None\n"
        "kc.CustomObjectsApi=kc.CoreV1Api=lambda *a,**k:None\n"
        "sys.path.insert(0,'agent-sandbox-platform/broker')\n"
        "import main;print(main.DEFAULT_PROFILE,main.PERSISTENT_MODE)\n"
    )
    out = subprocess.check_output([sys.executable, "-c", code], cwd=str(repo)).decode()
    assert "persistent per-user-pvc" in out
