#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only
#
# Live toolchain state for the workbench sandbox image
# (openspec/changes/2026-08-29-workbench-toolchain).
#
# Prints (1) the baked-in capability manifest, (2) a live delta — key tool
# versions re-probed now and the current dpkg count vs the build-time base
# count baked into the manifest footer — and (3) the workspace conventions
# rendered from the CONFIGURED workspace root (${WORKDIR:-/workspace}).
# Always exits 0; missing pieces degrade to notes, never errors.

set -u

MANIFEST="${SANDBOX_TOOLS_MANIFEST:-/usr/local/share/sandbox-capabilities.md}"
WORKDIR="${WORKDIR:-/workspace}"

if [ -r "$MANIFEST" ]; then
    cat "$MANIFEST"
else
    echo "(no capability manifest at $MANIFEST — base image built without one)"
fi

echo
echo "## Live state (probed now)"
probe() {
    # probe <label> <cmd...> — print "<label> <version>" when a version shows up.
    local label="$1"; shift
    local out
    out="$("$@" 2>&1 </dev/null | head -3)" || true
    local ver
    ver="$(printf '%s' "$out" | grep -oE '[0-9]+(\.[0-9]+)+' | head -1 || true)"
    if [ -n "$ver" ]; then
        echo "- $label $ver"
    else
        echo "- $label (no version reported)"
    fi
}
probe python3 python3 --version
probe R R --version
probe node node --version
probe pwsh pwsh --version
probe dotnet dotnet --version
probe wixl wixl --version
probe duckdb duckdb --version
probe soffice soffice --version
probe sqlite3 sqlite3 --version

BASE_COUNT="$(sed -n 's/^Base image dpkg packages: \([0-9]\+\)\.$/\1/p' "$MANIFEST" 2>/dev/null || true)"
if command -v dpkg-query >/dev/null 2>&1; then
    NOW_COUNT="$(dpkg-query -W -f '${db:Status-Abbrev} ${binary:Package}\n' 2>/dev/null \
        | grep -c '^ii ' || true)"
    if [ -n "$BASE_COUNT" ] && [ -n "$NOW_COUNT" ]; then
        echo "Installed dpkg packages now: $NOW_COUNT (base image: $BASE_COUNT)"
    fi
fi

echo
echo "## Workspace conventions"
echo "- Scratch/intermediate files belong in ${WORKDIR}/tmp — create it if missing (mkdir -p ${WORKDIR}/tmp); keep ${WORKDIR} root for deliverables."
echo "- /tmp is tmpfs and wiped on pod restart; ${WORKDIR} persists across sessions."
echo "- Persistent Python env: python3 -m venv ${WORKDIR}/.venv (survives pod restarts)."
echo "- Session-local Python deps: pip install --target /packages/py PKG, then PYTHONPATH=/packages/py."
echo "- npm user prefix: npm config set prefix /packages/npm."
echo "- sudo apt-get install PKG writes the ephemeral rootfs — reinstalls are needed after a pod restart."

exit 0
