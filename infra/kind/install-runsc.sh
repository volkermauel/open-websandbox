#!/usr/bin/env bash
# Install gVisor runsc + the containerd shim + the gvisor-bin sidecar directory on the
# HOST at /usr/local/bin, so a KIND node can bind-mount them in (see
# kind-config-gvisor.yaml). This is the "gVisor delivery" shared by CI (e2e.yml) and the
# local dev script (scripts/setup-kind-gvisor.sh) — single source of truth.
#
# Why host-mount instead of baking runsc into a custom node image: since 2026-07 gVisor
# releases are multi-file (runsc needs gvisor-bin/ next to itself), which broke the old
# bake-into-image build. Host-mounting avoids that entirely and matches the upstream
# agent-sandbox gVisor-in-KIND example. systrap needs no KVM, so runsc runs on any host.
#
# Override GVISOR_RELEASE (e.g. release/20260807.0) to pin a known-good runsc.
set -Eeuo pipefail

case "$(uname -m)" in
  x86_64) ARCH=x86_64 ;;
  aarch64) ARCH=aarch64 ;;
  *) echo "install-runsc: unsupported arch $(uname -m)" >&2; exit 1 ;;
esac

GVISOR_RELEASE="${GVISOR_RELEASE:-release/latest}"
URL="https://storage.googleapis.com/gvisor/releases/${GVISOR_RELEASE}/${ARCH}"

echo "== downloading gVisor (${GVISOR_RELEASE}, ${ARCH}) =="
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
cd "$TMP"
curl -fsSLO "${URL}/gvisor.tar.bz2"
curl -fsSLO "${URL}/gvisor.tar.bz2.sha512"
sha512sum -c gvisor.tar.bz2.sha512

# Extracts runsc, containerd-shim-runsc-v1, and gvisor-bin/ (the sidecar dir runsc
# looks up next to itself at runtime — must land under /usr/local/bin WITH runsc).
sudo tar -xjf gvisor.tar.bz2 -C /usr/local/bin
sudo chmod a+rx /usr/local/bin/runsc /usr/local/bin/containerd-shim-runsc-v1
sudo chmod -R a+rX /usr/local/bin/gvisor-bin

echo "== runsc installed =="
/usr/local/bin/runsc --version
