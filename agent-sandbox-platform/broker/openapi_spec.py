"""Curated OpenAPI 3.0 spec for the code-standard broker.

The broker is a transparent reverse proxy to per-user gVisor sandboxes. This module
describes the **LLM-facing method surface** (what Open WebUI exposes to the model as
callable tools), hand-written for clarity rather than auto-generated — FastAPI's
auto-spec would only show /healthz + the catch-all.

Identity & isolation headers (Authorization, X-User-Id, X-Session-Id, X-Persistence)
are injected by Open WebUI per session — the model must NOT fill them — so they are
documented in ``info.description`` / the bearer security scheme, NOT as per-operation
parameters (which would tempt the model to hallucinate values). The operations expose
only the functional inputs the model actually chooses (command, file path, ...).
"""

OPENAPI: dict = {
    "openapi": "3.0.3",
    "info": {
        "title": "code-standard sandbox",
        "version": "1.0.0",
        "description": (
            "An isolated, per-user Linux sandbox (gVisor) for running shell commands "
            "and managing files. Each chat session runs in its own workspace folder; "
            "commands execute as a non-root user with curated Python libraries "
            "(pandas, numpy, openpyxl, PyYAML, requests, ...) pre-installed and the "
            "ability to install additional packages (`pip install` / `npm install`) "
            "at runtime.\n\n"
            "**Identity & isolation headers** — injected by Open WebUI per session; "
            "the caller does NOT set these:\n"
            "- `Authorization: Bearer <token>` (required)\n"
            "- `X-User-Id` (required) — selects the user's sandbox (and, on the "
            "persistent profile, the per-user PVC).\n"
            "- `X-Session-Id` (required) — scopes each chat to its own workspace "
            "folder.\n"
            "- `X-Persistence: persistent` (optional) — mount a per-user PVC so files "
            "survive across sessions; default `ephemeral` discards files when the "
            "session ends.\n\n"
            "Prefer `execute_command` for any work; the file operations move artifacts "
            "in/out of the workspace."
        ),
    },
    "servers": [{
        "url": "http://owui-broker.agent-sandbox-system.svc.cluster.local:8080",
        "description": "in-cluster broker (Open WebUI overrides the base URL)",
    }],
    "security": [{"bearerAuth": []}],
    "components": {
        "securitySchemes": {
            "bearerAuth": {"type": "http", "scheme": "bearer",
                           "description": "Shared bearer token (injected by Open WebUI)."},
        },
        "schemas": {
            "ExecuteRequest": {
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {"type": "string", "description": (
                        "Shell command. Pipes, redirects, env vars, and chaining "
                        "(&&, ;, |) are supported. Runs non-interactively in the "
                        "session's workspace folder. State persists across calls in "
                        "the same session (files, installed packages).")},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "default": 300,
                                "description": "Max seconds before the process tree is killed."},
                },
            },
            "ExecuteResponse": {
                "type": "object",
                "properties": {
                    "stdout": {"type": "string"},
                    "stderr": {"type": "string"},
                    "exit_code": {"type": "integer"},
                    "timed_out": {"type": "boolean"},
                },
                "required": ["stdout", "stderr", "exit_code", "timed_out"],
            },
            "UploadResponse": {
                "type": "object",
                "properties": {"saved": {"type": "string"}, "bytes": {"type": "integer"}},
            },
            "DirEntry": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "is_dir": {"type": "boolean"},
                               "size": {"type": "integer"}},
            },
            "ListResponse": {
                "type": "object",
                "properties": {"path": {"type": "string"},
                               "entries": {"type": "array", "items": {"$ref": "#/components/schemas/DirEntry"}}},
            },
            "ExistsResponse": {
                "type": "object",
                "properties": {"exists": {"type": "boolean"},
                               "is_file": {"type": "boolean"}, "is_dir": {"type": "boolean"}},
            },
            "Error": {"type": "object", "properties": {"detail": {"type": "string"}}},
        },
    },
    "paths": {
        "/execute": {
            "post": {
                "operationId": "execute_command",
                "summary": "Run a shell command in the user's isolated sandbox",
                "description": (
                    "Execute a non-interactive shell command in the session's "
                    "workspace folder and return stdout, stderr, exit code, and "
                    "whether it timed out. Curated libraries are pre-installed; "
                    "`pip install`/`npm install` work for extras. Each command runs "
                    "as a non-root user inside a gVisor sandbox with no cluster "
                    "network egress except HTTPS to the public internet."),
                "requestBody": {"required": True, "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/ExecuteRequest"}}}},
                "responses": {
                    "200": {"description": "Command completed", "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/ExecuteResponse"}}}},
                    "400": {"description": "Bad request body", "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Error"}}}},
                    "504": {"description": "Sandbox not ready in time", "content": {"application/json": {
                        "schema": {"$ref": "#/components/schemas/Error"}}}},
                },
            },
        },
        "/upload": {
            "post": {
                "operationId": "upload_file",
                "summary": "Upload a file into the workspace",
                "description": "Upload a file (multipart) into the session's workspace folder.",
                "requestBody": {"required": True, "content": {"multipart/form-data": {"schema": {
                    "type": "object", "properties": {"file": {"type": "string", "format": "binary"}}}}}},
                "responses": {"200": {"description": "Uploaded", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/UploadResponse"}}}}},
            },
        },
        "/download/{file_path}": {
            "get": {
                "operationId": "download_file",
                "summary": "Download a file from the workspace",
                "parameters": [{"name": "file_path", "in": "path", "required": True,
                                "schema": {"type": "string"},
                                "description": "Path relative to the session workspace folder."}],
                "responses": {"200": {"description": "File bytes", "content": {"application/octet-stream": {
                    "schema": {"type": "string", "format": "binary"}}}}},
            },
        },
        "/list/{file_path}": {
            "get": {
                "operationId": "list_files",
                "summary": "List directory contents",
                "parameters": [{"name": "file_path", "in": "path", "required": True,
                                "schema": {"type": "string"},
                                "description": "Directory relative to the session workspace folder (use `.` for the folder root)."}],
                "responses": {"200": {"description": "Listing", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/ListResponse"}}}}},
            },
        },
        "/exists/{file_path}": {
            "get": {
                "operationId": "check_exists",
                "summary": "Check whether a path exists",
                "parameters": [{"name": "file_path", "in": "path", "required": True,
                                "schema": {"type": "string"}}],
                "responses": {"200": {"description": "Existence info", "content": {"application/json": {
                    "schema": {"$ref": "#/components/schemas/ExistsResponse"}}}}},
            },
        },
    },
}
