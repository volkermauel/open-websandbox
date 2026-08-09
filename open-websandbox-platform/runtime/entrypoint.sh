#!/bin/sh
# Prepare per-session writable dirs (emptyDir-mounted at runtime), then start the
# runtime server. Runs as uid 1000; the emptyDirs are fsGroup-owned (1000).
set -e
# Bound per-sandbox process count (RLIMIT_NPROC) — caps fork bombs. gVisor enforces
# it. dash's ulimit lacks -u, so use prlimit (util-linux); server.py also sets it.
prlimit --nproc="${MAX_PROCS:-256}" 2>/dev/null || true
mkdir -p "${PIP_CACHE_DIR:-$HOME/.cache/pip}" \
         "${NPM_CONFIG_PREFIX:-$HOME/.npm-global}" \
         "$HOME/.npm" \
         "${MAMBA_ROOT_PREFIX:-/packages}" \
         /workspace /tmp 2>/dev/null || true
# Pin --loop asyncio: uvloop (auto-selected when installed) hits a host-dependent
# SIGSEGV on Python 3.14 in uv_getaddrinfo/libuv (reproduced locally, not in CI). The
# runtime is a low-QPS sandbox API, so uvloop's throughput edge is marginal; the stdlib
# loop is cheap crash insurance. (#56) -- loop is the stdlib asyncio event loop.
exec uvicorn server:app --host 0.0.0.0 --port 8888 --loop asyncio --app-dir /app "$@"
