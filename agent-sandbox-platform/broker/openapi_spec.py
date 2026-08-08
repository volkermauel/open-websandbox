# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Curated OpenAPI 3.0 spec for the code-standard broker (open-sandbox runtime API).

The broker is an authenticated reverse proxy in front of per-chat gVisor
sandboxes. This module describes the **complete client-facing method surface**:
the runtime operations proxied at ``/execute``, ``/files/*`` and
``/api/terminals/*`` plus the endpoints the broker serves itself (``/healthz``,
``/api/config``, ``/api/status``). Hand-written for clarity rather than
auto-generated — FastAPI's auto-spec would only show ``/healthz`` + the catch-all.

Served at ``/openapi.json`` (rendered at ``/docs``) so Open WebUI can discover
the model's callable tools.

Identity & isolation headers (``Authorization``, ``X-User-Id``, ``X-Session-Id``,
``X-Persistence``) are injected by Open WebUI / the broker per session — the
model must NOT fill them — so they live in ``info.description`` / the bearer
security scheme, NOT as per-operation parameters (which would tempt the model to
hallucinate values). Operations expose only the functional inputs the caller
chooses (command, file path, ...).

``info.version`` is derived at import from the Helm chart ``appVersion`` (read
from ``../chart/Chart.yaml``) so the served spec and the chart never silently
drift; if the chart is absent it falls back to the ``OPEN_WEBSANDBOX_VERSION``
env override and then a sentinel default. The surface is additive within a
minor — operations are appended, never silently removed or renamed until a
major bump.
"""

import os
from pathlib import Path

import yaml

# Helm chart whose ``appVersion`` is the single source of truth for
# ``info.version``. Resolved relative to this module so the lookup works from the
# broker deployment and from an ad-hoc ``import openapi_spec`` in tests.
_CHART_PATH = Path(__file__).resolve().parents[1] / "chart" / "Chart.yaml"

# Sentinel used only when neither the chart nor an env override is available.
_DEFAULT_API_VERSION = "0.0.0"


def _chart_appversion(path: Path) -> str | None:
    """Return the Helm chart ``appVersion`` from *path*, or ``None`` if unreadable.

    The chart may be absent (e.g. the module imported standalone, or packaged
    without the chart), malformed, or lack ``appVersion``; every such case yields
    ``None`` so an OpenAPI-version lookup can never crash the broker import.
    """
    try:
        data = yaml.safe_load(path.read_text(encoding="utf-8"))
    except (OSError, yaml.YAMLError):
        return None
    if not isinstance(data, dict):
        return None
    version = data.get("appVersion")
    if version is None or isinstance(version, bool):
        return None
    # Tolerate an unquoted numeric ``appVersion`` (e.g. ``appVersion: 5``).
    if isinstance(version, (int, float)):
        version = str(version)
    if isinstance(version, str):
        version = version.strip()
        return version or None
    return None


def _resolve_openapi_version(chart_path: Path | None = None) -> str:
    """Resolve the OpenAPI ``info.version`` from a single source of truth.

    Precedence (matches the module docstring): the Helm chart ``appVersion`` is
    authoritative; if the chart is absent/unreadable, fall back to the
    ``OPEN_WEBSANDBOX_VERSION`` env override, then to :data:`_DEFAULT_API_VERSION`.
    """
    path = chart_path if chart_path is not None else _CHART_PATH
    return (
        _chart_appversion(path)
        or os.environ.get("OPEN_WEBSANDBOX_VERSION")
        or _DEFAULT_API_VERSION
    )

# Built incrementally below so the module stays importable (valid Python) at
# every step. ``OPENAPI`` is the canonical attribute imported by broker/main.py
# (``from openapi_spec import OPENAPI``); ``SPEC`` is a convenience alias.
OPENAPI: dict = {
    "openapi": "3.0.3",
    "info": {
        "title": "open-sandbox runtime API",
        "version": _resolve_openapi_version(),
        "description": (
            "Open-sandbox runtime API surface, served by the code-standard broker "
            "(an authenticated reverse proxy in front of per-chat gVisor sandboxes). "
            "The broker proxies the runtime at `/execute`, `/files/*` and "
            "`/api/terminals/*`, and serves `/healthz`, `/api/config` and `/api/status` "
            "directly. Each chat session runs in its own isolated workspace folder; "
            "commands execute as a non-root user with curated Python libraries "
            "(pandas, numpy, openpyxl, PyYAML, requests, ...) pre-installed and the "
            "ability to install more at runtime — `micromamba install -c conda-forge "
            "<pkg>` (system libs + CLI tools like ffmpeg, jq, graphviz, compilers) and "
            "`pip install` for Python (non-root, into /packages).\n\n"
            "Prefer `execute_command` for any work; the file operations move artifacts "
            "in/out of the workspace, and `/files/archive` + `/files/upload` back the "
            "broker's own staging->chat migration.\n\n"
            "**Identity & isolation headers** — injected by Open WebUI / the broker per "
            "session; the caller does NOT set these:\n"
            "- `Authorization: Bearer <token>` (required)\n"
            "- `X-User-Id` (required) — selects the user's sandbox (and, on the "
            "persistent profile, the per-user PVC).\n"
            "- `X-Session-Id` (required) — scopes each chat to its own workspace folder; "
            "also the interactive-terminal id.\n"
            "- `X-Persistence: persistent` (optional) — mount a per-user PVC so files "
            "survive across sessions; default `ephemeral` discards files when the session "
            "ends.\n\n"
            "**Versioning**: `info.version` mirrors the Helm chart `appVersion`. The "
            "surface is additive within a minor — new operations are added, existing ones "
            "are not silently removed or renamed until a major bump."
        ),
    },
    "servers": [{
        "url": "http://owui-broker.agent-sandbox-system.svc.cluster.local:8080",
        "description": "in-cluster broker (Open WebUI overrides the base URL)",
    }],
    "tags": [
        {"name": "Broker", "description": "Endpoints served by the broker itself."},
        {"name": "Execute", "description": "Run shell commands in the sandbox."},
        {"name": "Files", "description": "Workspace filesystem operations (proxied)."},
        {"name": "Terminals", "description": "Interactive PTY terminals (proxied)."},
    ],
    "security": [{"bearerAuth": []}],
    "components": {
        "securitySchemes": {
            "bearerAuth": {"type": "http", "scheme": "bearer",
                           "description": "Shared bearer token (injected by Open WebUI)."},
        },
        "schemas": {},
    },
    "paths": {},
}

S = OPENAPI["components"]["schemas"]
P = OPENAPI["paths"]

# Convenience alias so the spec is reachable as `openapi_spec.SPEC`.
SPEC = OPENAPI

# --- schemas ---
S["Error"] = {"type": "object", "properties": {"detail": {"type": "string"}}}
S["ExecuteRequest"] = {
    "type": "object", "required": ["command"],
    "properties": {
        "command": {"type": "string", "description": (
            "Shell command. Pipes, redirects, env vars, and chaining (&&, ;, |) are "
            "supported. Runs non-interactively in the session's workspace folder. State "
            "persists across calls in the same session (files, installed packages).")},
        "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "default": 120,
                    "description": "Max seconds before the process tree is killed."},
    },
}
S["ExecuteResponse"] = {
    "type": "object",
    "properties": {
        "stdout": {"type": "string"}, "stderr": {"type": "string"},
        "exit_code": {"type": "integer"}, "timed_out": {"type": "boolean"},
    },
    "required": ["stdout", "stderr", "exit_code", "timed_out"],
}
S["HealthResponse"] = {"type": "object", "properties": {"status": {"type": "string"}}, "required": ["status"]}
S["CwdRequest"] = {
    "type": "object", "required": ["path"],
    "properties": {"path": {"type": "string", "description": "Directory to switch to (workspace-relative or absolute inside the workspace)."}},
}
S["CwdResponse"] = {
    "type": "object", "properties": {"cwd": {"type": "string"}, "home": {"type": "string"}}, "required": ["cwd"],
}
S["FileEntry"] = {
    "type": "object",
    "properties": {
        "name": {"type": "string"},
        "type": {"type": "string", "enum": ["file", "directory"]},
        "size": {"type": "integer"}, "modified": {"type": "number", "description": "mtime epoch seconds."},
    },
    "required": ["name", "type", "size", "modified"],
}
S["FilesListResponse"] = {
    "type": "object",
    "properties": {"dir": {"type": "string"}, "entries": {"type": "array", "items": {"$ref": "#/components/schemas/FileEntry"}}},
    "required": ["dir", "entries"],
}
S["FileReadResponse"] = {
    "type": "object",
    "properties": {"path": {"type": "string"}, "total_lines": {"type": "integer"}, "content": {"type": "string"}},
    "required": ["path", "total_lines", "content"],
}
S["WriteRequest"] = {
    "type": "object", "required": ["path", "content"],
    "properties": {"path": {"type": "string"}, "content": {"type": "string", "description": "Full file contents (UTF-8)."}},
}
S["FileWriteResponse"] = {
    "type": "object", "properties": {"path": {"type": "string"}, "size": {"type": "integer"}}, "required": ["path", "size"],
}
S["PathRequest"] = {"type": "object", "required": ["path"], "properties": {"path": {"type": "string"}}}
S["PathResponse"] = {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
S["MoveRequest"] = {
    "type": "object", "required": ["source", "destination"],
    "properties": {"source": {"type": "string"}, "destination": {"type": "string"}},
}
S["MoveResponse"] = {
    "type": "object", "properties": {"source": {"type": "string"}, "destination": {"type": "string"}}, "required": ["source", "destination"],
}
S["DeleteResponse"] = {
    "type": "object",
    "properties": {"path": {"type": "string"}, "type": {"type": "string", "enum": ["file", "directory"]}},
    "required": ["path", "type"],
}
S["ReplacementChunk"] = {
    "type": "object", "required": ["target", "replacement"],
    "properties": {
        "target": {"type": "string"}, "replacement": {"type": "string"},
        "start_line": {"type": "integer", "minimum": 1, "description": "Optional 1-based start line scoping the replacement."},
        "end_line": {"type": "integer", "minimum": 1, "description": "Optional 1-based end (inclusive) line scoping the replacement."},
        "allow_multiple": {"type": "boolean", "default": False, "description": "Allow replacing more than one occurrence of `target`."},
    },
}
S["ReplaceRequest"] = {
    "type": "object", "required": ["path", "replacements"],
    "properties": {
        "path": {"type": "string"},
        "replacements": {"type": "array", "items": {"$ref": "#/components/schemas/ReplacementChunk"}},
    },
}
S["GrepMatch"] = {
    "type": "object",
    "properties": {"file": {"type": "string"}, "line": {"type": "integer"}, "content": {"type": "string"}},
    "required": ["file", "line", "content"],
}
S["GrepResponse"] = {
    "type": "object",
    "properties": {
        "query": {"type": "string"}, "path": {"type": "string"},
        "matches": {"type": "array", "items": {"$ref": "#/components/schemas/GrepMatch"}}, "truncated": {"type": "boolean"},
    },
    "required": ["query", "path", "matches", "truncated"],
}
S["GlobMatch"] = {
    "type": "object",
    "properties": {"path": {"type": "string"}, "type": {"type": "string", "enum": ["file", "directory"]}, "size": {"type": "integer"}, "modified": {"type": "number"}},
    "required": ["path", "type", "size", "modified"],
}
S["GlobResponse"] = {
    "type": "object",
    "properties": {
        "pattern": {"type": "string"}, "path": {"type": "string"},
        "matches": {"type": "array", "items": {"$ref": "#/components/schemas/GlobMatch"}}, "truncated": {"type": "boolean"},
    },
    "required": ["pattern", "path", "matches", "truncated"],
}
S["ArchiveRequest"] = {
    "type": "object", "required": ["paths"],
    "properties": {"paths": {"type": "array", "items": {"type": "string"}, "description": "Files/directories (workspace-relative) to zip."}},
}
S["FilesUploadResponse"] = {"type": "object", "properties": {"path": {"type": "string"}, "size": {"type": "integer"}}, "required": ["path", "size"]}
S["ToolUploadResponse"] = {"type": "object", "properties": {"saved": {"type": "string"}, "bytes": {"type": "integer"}}, "required": ["saved", "bytes"]}
S["DirEntry"] = {
    "type": "object", "properties": {"name": {"type": "string"}, "is_dir": {"type": "boolean"}, "size": {"type": "integer"}}, "required": ["name", "is_dir", "size"],
}
S["ListResponse"] = {
    "type": "object", "properties": {"path": {"type": "string"}, "entries": {"type": "array", "items": {"$ref": "#/components/schemas/DirEntry"}}}, "required": ["path", "entries"],
}
S["ExistsResponse"] = {
    "type": "object", "properties": {"exists": {"type": "boolean"}, "is_file": {"type": "boolean"}, "is_dir": {"type": "boolean"}}, "required": ["exists", "is_file", "is_dir"],
}
S["Terminal"] = {
    "type": "object", "properties": {"id": {"type": "string"}, "created_at": {"type": "string", "format": "date-time"}, "pid": {"type": "integer"}}, "required": ["id", "created_at", "pid"],
}
S["TerminalStatusResponse"] = {"type": "object", "properties": {"status": {"type": "string"}}, "required": ["status"]}
S["ConfigResponse"] = {
    "type": "object", "required": ["features"],
    "properties": {"features": {
        "type": "object", "required": ["terminal", "notebooks", "desktop"],
        "properties": {"terminal": {"type": "boolean"}, "notebooks": {"type": "boolean"}, "desktop": {"type": "boolean"}},
    }},
}
S["StatusResponse"] = {
    "type": "object", "properties": {"active_pods": {"type": "integer"}, "max_pods": {"type": "integer"}, "pods": {"type": "array", "items": {"type": "object"}}}, "required": ["active_pods", "max_pods", "pods"],
}

# --- paths ---
P["/healthz"] = {
    "get": {
        "tags": ["Broker"], "operationId": "healthz",
        "summary": "Broker liveness", "description": "Broker process is up (always 200; never proxied).",
        "responses": {"200": {"description": "Broker alive", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/HealthResponse"}}}}},
    },
}
P["/execute"] = {
    "post": {
        "tags": ["Execute"], "operationId": "execute_command",
        "summary": "Run a shell command in the user's isolated sandbox",
        "description": (
            "Execute a non-interactive shell command in the session's workspace "
            "folder and return stdout, stderr, exit code, and whether it timed out. "
            "Curated libraries are pre-installed; `micromamba install -c conda-forge "
            "<pkg>` (system libs/tools) and `pip install` work for extras (non-root, "
            "into /packages). Each command runs as a non-root user inside a gVisor "
            "sandbox with egress limited to public HTTPS."),
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ExecuteRequest"}}}},
        "responses": {
            "200": {"description": "Command completed (exit_code reflects the process; a runtime error is reported via exit_code 1, not an HTTP error)", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ExecuteResponse"}}}},
            "400": {"description": "Bad request body", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "504": {"description": "Sandbox not ready in time", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/cwd"] = {
    "get": {
        "tags": ["Files"], "operationId": "get_cwd", "summary": "Get the current workspace directory",
        "responses": {"200": {"description": "Current working directory", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/CwdResponse"}}}}},
    },
    "post": {
        "tags": ["Files"], "operationId": "set_cwd", "summary": "Set the current workspace directory",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/CwdRequest"}}}},
        "responses": {
            "200": {"description": "Directory changed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/CwdResponse"}}}},
            "404": {"description": "Directory not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/list"] = {
    "get": {
        "tags": ["Files"], "operationId": "list_directory",
        "summary": "List directory entries", "description": "Returns typed entries (file/directory) with size and mtime.",
        "parameters": [{"name": "directory", "in": "query", "required": False,
                        "schema": {"type": "string", "default": "."},
                        "description": "Directory relative to the workspace (default `.`)."}],
        "responses": {
            "200": {"description": "Listing", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/FilesListResponse"}}}},
            "404": {"description": "Directory not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "500": {"description": "List failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/read"] = {
    "get": {
        "tags": ["Files"], "operationId": "read_file",
        "summary": "Read a text file (or return image bytes)",
        "description": "Text files return JSON `{path,total_lines,content}`; image files return the raw image bytes with the guessed media type.",
        "parameters": [{"name": "path", "in": "query", "required": True, "schema": {"type": "string"}}],
        "responses": {
            "200": {"description": "File contents (text JSON or image bytes)", "content": {
                "application/json": {"schema": {"$ref": "#/components/schemas/FileReadResponse"}},
                "image/*": {"schema": {"type": "string", "format": "binary"}}}},
            "404": {"description": "File not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "500": {"description": "Read failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/write"] = {
    "post": {
        "tags": ["Files"], "operationId": "write_file", "summary": "Write a text file (overwrite)",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/WriteRequest"}}}},
        "responses": {
            "200": {"description": "Written", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/FileWriteResponse"}}}},
            "400": {"description": "Write failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/mkdir"] = {
    "post": {
        "tags": ["Files"], "operationId": "make_directory", "summary": "Create a directory (recursive)",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/PathRequest"}}}},
        "responses": {
            "200": {"description": "Created", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/PathResponse"}}}},
            "400": {"description": "Create failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/move"] = {
    "post": {
        "tags": ["Files"], "operationId": "move_path", "summary": "Move/rename a file or directory",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/MoveRequest"}}}},
        "responses": {
            "200": {"description": "Moved", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/MoveResponse"}}}},
            "400": {"description": "Move failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "404": {"description": "Source path not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "409": {"description": "Destination already exists", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/delete"] = {
    "delete": {
        "tags": ["Files"], "operationId": "delete_path", "summary": "Delete a file or directory",
        "parameters": [{"name": "path", "in": "query", "required": True, "schema": {"type": "string"}}],
        "responses": {
            "200": {"description": "Deleted", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/DeleteResponse"}}}},
            "400": {"description": "Delete failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "404": {"description": "Path not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/replace"] = {
    "post": {
        "tags": ["Files"], "operationId": "replace_in_file", "summary": "Apply targeted string replacements in a file",
        "description": "Replaces each `target` with `replacement` (optionally scoped to a 1-based line range). Fails if a target is absent, or if it occurs more than once without `allow_multiple`.",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ReplaceRequest"}}}},
        "responses": {
            "200": {"description": "Replacements applied", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/FileWriteResponse"}}}},
            "400": {"description": "Target not found / ambiguous / write failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "404": {"description": "File not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/grep"] = {
    "get": {
        "tags": ["Files"], "operationId": "grep_files", "summary": "Search file contents",
        "parameters": [
            {"name": "query", "in": "query", "required": True, "schema": {"type": "string"}},
            {"name": "path", "in": "query", "required": False, "schema": {"type": "string", "default": "."}},
            {"name": "regex", "in": "query", "required": False, "schema": {"type": "boolean", "default": True}, "description": "Treat `query` as a regex (default) vs. a literal string."},
            {"name": "case_insensitive", "in": "query", "required": False, "schema": {"type": "boolean", "default": False}},
            {"name": "include", "in": "query", "required": False, "schema": {"type": "array", "items": {"type": "string"}}, "description": "Optional fnmatch filename filters (repeatable)."},
            {"name": "max_results", "in": "query", "required": False, "schema": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}},
        ],
        "responses": {
            "200": {"description": "Match list", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/GrepResponse"}}}},
            "400": {"description": "Invalid regex", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "404": {"description": "Search path not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/glob"] = {
    "get": {
        "tags": ["Files"], "operationId": "glob_files", "summary": "Find files/directories by glob pattern",
        "parameters": [
            {"name": "pattern", "in": "query", "required": True, "schema": {"type": "string"}},
            {"name": "path", "in": "query", "required": False, "schema": {"type": "string", "default": "."}},
            {"name": "type", "in": "query", "required": False, "schema": {"type": "string", "enum": ["any", "file", "directory"], "default": "any"}},
            {"name": "max_results", "in": "query", "required": False, "schema": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}},
        ],
        "responses": {
            "200": {"description": "Match list", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/GlobResponse"}}}},
            "404": {"description": "Search directory not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/upload"] = {
    "post": {
        "tags": ["Files"], "operationId": "upload_to_directory",
        "summary": "Upload a file into a chosen directory",
        "description": "Multipart upload; creates the target directory if missing. Backs the broker's staging->chat migration.",
        "requestBody": {"required": True, "content": {"multipart/form-data": {"schema": {
            "type": "object", "required": ["file"], "properties": {
                "file": {"type": "string", "format": "binary"},
                "directory": {"type": "string", "default": "", "description": "Target directory relative to the workspace (default workspace root)."},
            }}}}},
        "responses": {
            "200": {"description": "Uploaded", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/FilesUploadResponse"}}}},
            "400": {"description": "Upload failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/archive"] = {
    "post": {
        "tags": ["Files"], "operationId": "archive_paths", "summary": "Zip one or more files/directories",
        "description": "Returns a `application/zip` download with a Content-Disposition filename (basename of the single path, or `download.zip`).",
        "requestBody": {"required": True, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ArchiveRequest"}}}},
        "responses": {
            "200": {"description": "ZIP archive", "content": {"application/zip": {"schema": {"type": "string", "format": "binary"}}}},
            "400": {"description": "No paths provided", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "404": {"description": "A requested path was not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/files/view"] = {
    "get": {
        "tags": ["Files"], "operationId": "view_file", "summary": "Download a file's raw bytes",
        "description": "Streams the file with the guessed media type (OWUI downloadFileBlob -> res.blob()).",
        "parameters": [{"name": "path", "in": "query", "required": True, "schema": {"type": "string"}}],
        "responses": {
            "200": {"description": "File bytes", "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}},
            "404": {"description": "File not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/upload"] = {
    "post": {
        "tags": ["Files"], "operationId": "upload_file", "summary": "Upload a file to the workspace root (tool surface)",
        "description": "Thin handler backing the model's `upload_file` tool (uploads to the workspace root).",
        "requestBody": {"required": True, "content": {"multipart/form-data": {"schema": {
            "type": "object", "required": ["file"], "properties": {"file": {"type": "string", "format": "binary"}},
        }}}},
        "responses": {
            "200": {"description": "Uploaded", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ToolUploadResponse"}}}},
            "400": {"description": "Upload failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/download/{file_path}"] = {
    "get": {
        "tags": ["Files"], "operationId": "download_file", "summary": "Download a file by path (tool surface)",
        "parameters": [{"name": "file_path", "in": "path", "required": True, "schema": {"type": "string"}, "description": "Workspace-relative file path."}],
        "responses": {
            "200": {"description": "File bytes", "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}},
            "404": {"description": "File not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/list/{file_path}"] = {
    "get": {
        "tags": ["Files"], "operationId": "list_files", "summary": "List a directory by path (tool surface)",
        "parameters": [{"name": "file_path", "in": "path", "required": True, "schema": {"type": "string"}, "description": "Workspace-relative directory path (empty/`.` for root)."}],
        "responses": {
            "200": {"description": "Listing", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ListResponse"}}}},
            "404": {"description": "Directory not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "500": {"description": "List failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
}
P["/exists/{file_path}"] = {
    "get": {
        "tags": ["Files"], "operationId": "check_exists", "summary": "Check whether a path exists (tool surface)",
        "parameters": [{"name": "file_path", "in": "path", "required": True, "schema": {"type": "string"}}],
        "responses": {"200": {"description": "Existence probe", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ExistsResponse"}}}}},
    },
}
P["/api/terminals"] = {
    "post": {
        "tags": ["Terminals"], "operationId": "create_terminal",
        "summary": "Create an interactive terminal",
        "description": ("Forks a shell on a PTY scoped to the session workspace. "
            "X-Session-Id (broker-injected) selects the id; the id is also the "
            "path segment for the streaming WebSocket."),
        "responses": {
            "200": {"description": "Terminal created", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Terminal"}}}},
            "429": {"description": "Per-pod terminal cap reached", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
            "503": {"description": "PTY spawn failed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
    "get": {
        "tags": ["Terminals"], "operationId": "list_terminals", "summary": "List live terminals",
        "responses": {
            "200": {"description": "Live terminals", "content": {"application/json": {"schema": {"type": "array", "items": {"$ref": "#/components/schemas/Terminal"}}}}},
        },
    },
}
P["/api/terminals/{session_id}"] = {
    "x-websocket": {
        "summary": "Interactive terminal stream (WebSocket)",
        "description": (
            "Upgrade to a WebSocket for bidirectional PTY IO. The broker "
            "authenticates the first message, resolves the sandbox, then proxies "
            "the WS to the runtime PTY. Protocol: binary frames are stdin/stdout "
            "bytes; text frames carry JSON control messages "
            "`{\"type\":\"resize\",\"cols\":N,\"rows\":N}` (and, when the "
            "runtime API key is set, a first `{\"type\":\"auth\",\"token\":...}` "
            "handshake). Identity/session arrive via user_id/session_id query "
            "params or headers (browser WS clients cannot set arbitrary headers)."),
    },
    "get": {
        "tags": ["Terminals"], "operationId": "get_terminal", "summary": "Inspect a terminal",
        "parameters": [{"name": "session_id", "in": "path", "required": True, "schema": {"type": "string"}}],
        "responses": {
            "200": {"description": "Terminal details", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Terminal"}}}},
            "404": {"description": "Terminal not found", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
        },
    },
    "delete": {
        "tags": ["Terminals"], "operationId": "delete_terminal", "summary": "Kill a terminal",
        "parameters": [{"name": "session_id", "in": "path", "required": True, "schema": {"type": "string"}}],
        "responses": {
            "200": {"description": "Terminal killed", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/TerminalStatusResponse"}}}},
        },
    },
}
P["/api/config"] = {
    "get": {
        "tags": ["Broker"], "operationId": "terminal_config",
        "summary": "Feature discovery (terminal UI connection gate)",
        "description": "Static (never proxied); served Bearer-only, matching open-terminal-k8s-proxy. The OWUI terminal UI treats a 200 here as the connection-success signal.",
        "responses": {
            "200": {"description": "Feature flags", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/ConfigResponse"}}}},
        },
    },
}
P["/api/status"] = {
    "get": {
        "tags": ["Broker"], "operationId": "terminal_status",
        "summary": "Operator telemetry",
        "description": "Static (never proxied) sandbox pod telemetry.",
        "responses": {
            "200": {"description": "Pod telemetry", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/StatusResponse"}}}},
        },
    },
}
