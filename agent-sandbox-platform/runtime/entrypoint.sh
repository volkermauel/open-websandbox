#!/bin/sh
# Prepare per-session writable dirs (emptyDir-mounted at runtime), then start the
# runtime server. Runs as uid 1000; the emptyDirs are fsGroup-owned (1000).
set -e
mkdir -p "${PIP_CACHE_DIR:-$HOME/.cache/pip}" \
         "${NPM_CONFIG_PREFIX:-$HOME/.npm-global}" \
         "$HOME/.npm" \
         "${MAMBA_ROOT_PREFIX:-/packages}" \
         /workspace /tmp 2>/dev/null || true
exec uvicorn server:app --host 0.0.0.0 --port 8888 --app-dir /app "$@"
