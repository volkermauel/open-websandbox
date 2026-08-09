# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""``info.version`` derivation for the served OpenAPI spec (issue #21).

The served OpenAPI ``info.version`` must track the Helm chart ``appVersion``
(the single source of truth) so the two never silently drift, and it must fall
back gracefully when the chart is absent/unreadable. These tests exercise both
the parser helper and the precedence chain (chart -> env -> default).
"""
from __future__ import annotations

import sys
from pathlib import Path

import yaml

# ``openapi_spec`` is a flat module in the broker dir (imported as
# ``from openapi_spec import OPENAPI`` by broker/main.py), not an installed
# package — put its directory on sys.path the same way the broker conftest does.
_BROKER_DIR = Path(__file__).resolve().parents[3] / "open-websandbox-platform" / "broker"
if str(_BROKER_DIR) not in sys.path:
    sys.path.insert(0, str(_BROKER_DIR))

import openapi_spec  # type: ignore[import-not-found]  # noqa: E402

_REPO_ROOT = Path(__file__).resolve().parents[3]
_CHART_YAML = _REPO_ROOT / "open-websandbox-platform" / "chart" / "Chart.yaml"


def _read_appversion_independently() -> str:
    """Read ``appVersion`` straight from Chart.yaml, bypassing the helper."""
    data = yaml.safe_load(_CHART_YAML.read_text(encoding="utf-8"))
    assert isinstance(data, dict)
    return str(data["appVersion"])


# --- live spec is driven by the chart (no hardcoding) -----------------------------
def test_served_version_equals_chart_appversion():
    """The built OPENAPI dict version equals the chart's appVersion."""
    expected = _read_appversion_independently()
    assert openapi_spec.OPENAPI["info"]["version"] == expected
    assert expected  # non-empty, not a stale placeholder


def test_served_version_uses_real_chart_path():
    """The module-level default chart path points at the repo's Chart.yaml."""
    assert openapi_spec._CHART_PATH == _CHART_YAML


# --- _chart_appversion parser -----------------------------------------------------
def test_chart_appversion_reads_real_chart():
    assert openapi_spec._chart_appversion(_CHART_YAML) == _read_appversion_independently()


def test_chart_appversion_missing_file_returns_none(tmp_path):
    assert openapi_spec._chart_appversion(tmp_path / "missing.yaml") is None


def test_chart_appversion_no_appversion_key_returns_none(tmp_path):
    chart = tmp_path / "Chart.yaml"
    chart.write_text("apiVersion: v2\nname: x\n", encoding="utf-8")
    assert openapi_spec._chart_appversion(chart) is None


def test_chart_appversion_malformed_yaml_returns_none(tmp_path):
    chart = tmp_path / "Chart.yaml"
    # Unterminated single-quoted scalar -> yaml.ScannerError (a YAMLError).
    chart.write_text("apiVersion: 'unterminated quote\n", encoding="utf-8")
    assert openapi_spec._chart_appversion(chart) is None


def test_chart_appversion_empty_value_returns_none(tmp_path):
    chart = tmp_path / "Chart.yaml"
    chart.write_text('apiVersion: v2\nappVersion: "  "\n', encoding="utf-8")
    assert openapi_spec._chart_appversion(chart) is None


def test_chart_appversion_unquoted_numeric_is_coerced(tmp_path):
    chart = tmp_path / "Chart.yaml"
    chart.write_text("apiVersion: v2\nappVersion: 5\n", encoding="utf-8")
    assert openapi_spec._chart_appversion(chart) == "5"


# --- _resolve_openapi_version precedence (chart -> env -> default) ----------------
def test_resolve_prefers_chart_when_present(tmp_path):
    chart = tmp_path / "Chart.yaml"
    chart.write_text('apiVersion: v2\nappVersion: "9.9.9"\n', encoding="utf-8")
    assert openapi_spec._resolve_openapi_version(chart_path=chart) == "9.9.9"


def test_resolve_falls_back_to_env_when_chart_absent(monkeypatch, tmp_path):
    monkeypatch.setenv("OPEN_WEBSANDBOX_VERSION", "2.3.4")
    missing = tmp_path / "absent" / "Chart.yaml"
    assert openapi_spec._resolve_openapi_version(chart_path=missing) == "2.3.4"


def test_resolve_falls_back_to_default(monkeypatch, tmp_path):
    monkeypatch.delenv("OPEN_WEBSANDBOX_VERSION", raising=False)
    missing = tmp_path / "absent" / "Chart.yaml"
    got = openapi_spec._resolve_openapi_version(chart_path=missing)
    assert got == openapi_spec._DEFAULT_API_VERSION
    assert got  # non-empty sentinel


def test_resolve_ignores_empty_env(monkeypatch, tmp_path):
    monkeypatch.setenv("OPEN_WEBSANDBOX_VERSION", "")
    missing = tmp_path / "absent" / "Chart.yaml"
    assert openapi_spec._resolve_openapi_version(chart_path=missing) == (
        openapi_spec._DEFAULT_API_VERSION
    )
