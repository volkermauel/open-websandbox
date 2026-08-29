#!/usr/bin/env bash
#
# install-gvisor-node.sh — idempotently STAGE the gVisor runsc containerd handler
# on a snap-packaged MicroK8s node. Safe to run on a live node: it only writes
# files and edits the containerd *template*. It does NOT restart containerd, so
# nothing changes until you run activate-gvisor-node.sh.
#
# Run ON the node as root, or pipe over SSH from your workstation:
#     ssh ubuntu@<node> 'sudo bash -s' < install-gvisor-node.sh
#
# Tested on MicroK8s classic snap (unconfined), containerd 2.2.3, x86_64.
# See README.md for prerequisites, the online-safety model, and the CNPG caveat.
#
set -euo pipefail

GVISOR_RELEASE="${GVISOR_RELEASE:-release/latest}"      # pin e.g. release-20260727.0 for reproducibility
ARCH="${ARCH:-$(uname -m)}"                             # gvisor publishes x86_64 (and aarch64)
TEMPLATE="${TEMPLATE:-/var/snap/microk8s/current/args/containerd-template.toml}"
RENDERED="${RENDERED:-/var/snap/microk8s/current/args/containerd.toml}"
RUNSC_PLATFORM="${RUNSC_PLATFORM:-systrap}"             # systrap = no nested KVM needed; kvm = needs /dev/kvm
RUNSC_CFG="${RUNSC_CFG:-/etc/runsc/config.toml}"

echo "==> gVisor node STAGING (snap MicroK8s)"
echo "    release=$GVISOR_RELEASE arch=$ARCH platform=$RUNSC_PLATFORM"

# --- sanity: this looks like a snap MicroK8s node -----------------------------
if [ ! -f "$TEMPLATE" ]; then
  echo "!! containerd template not found at $TEMPLATE" >&2
  echo "   (this must be a snap MicroK8s node; the template is rendered into containerd.toml)" >&2
  exit 1
fi
if [ "${ARCH}" != "x86_64" ] && [ "${ARCH}" != "aarch64" ]; then
  echo "!! unsupported arch '$ARCH' (gvisor publishes x86_64 / aarch64)" >&2; exit 1
fi

# --- 1. binaries: runsc + containerd-shim-runsc-v1 -> /usr/local/bin ----------
need=0
for b in runsc containerd-shim-runsc-v1; do [ -x "/usr/local/bin/$b" ] || need=1; done
if [ "$need" = "1" ]; then
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  base="https://storage.googleapis.com/gvisor/releases/${GVISOR_RELEASE}/${ARCH}"
  for b in runsc containerd-shim-runsc-v1; do
    echo "    fetching $b"
    if command -v wget >/dev/null; then wget -q -O "$tmp/$b" "$base/$b"
    elif command -v curl >/dev/null; then curl -fsSL -o "$tmp/$b" "$base/$b"
    else python3 -c "import urllib.request,sys; urllib.request.urlretrieve(sys.argv[1],sys.argv[2])" "$base/$b" "$tmp/$b"; fi
  done
  install -m 0755 "$tmp/runsc" "$tmp/containerd-shim-runsc-v1" /usr/local/bin/
  echo "    installed -> /usr/local/bin/{runsc,containerd-shim-runsc-v1}"
else
  echo "    binaries already present in /usr/local/bin"
fi
/usr/local/bin/runsc --version 2>&1 | head -1 | sed 's/^/    /'

# --- 2. runsc config ----------------------------------------------------------
mkdir -p "$(dirname "$RUNSC_CFG")"
# runsc flags go under [runsc_config] as STRING values — this file is the
# containerd-shim-runsc-v1 config (options.ConfigPath), not a bare runsc config.
printf '[runsc_config]\n  platform = "%s"\n  allow-suid = "true"\n' "$RUNSC_PLATFORM" > "$RUNSC_CFG"
echo "    wrote $RUNSC_CFG (shim config: platform=$RUNSC_PLATFORM, allow_suid=true for the workbench sudo-apt surface)"

# --- 3. inject runsc handler into the containerd TEMPLATE (idempotent) --------
#   MicroK8s renders containerd.toml from containerd-template.toml on daemon
#   restart. Editing the rendered file is pointless (overwritten). The shipped
#   template already contains runc / nvidia / kata handler blocks; we mirror the
#   kata block by anchoring on its BinaryName line.
if grep -q 'containerd\.runtimes\.runsc\]' "$TEMPLATE"; then
  echo "    runsc handler already present in template"
else
  python3 - "$TEMPLATE" <<'PY'
import sys
p = sys.argv[1]
t = open(p).read()
anchor = 'BinaryName = "kata-runtime"'   # present in the shipped MicroK8s template
assert anchor in t, f"anchor '{anchor}' not found in {p}; template format changed — add runsc manually"
block = '''

       [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runsc]
         runtime_type = "io.containerd.runsc.v1"
         [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runsc.options]
           BinaryName = "/usr/local/bin/runsc"
           ConfigPath = "/etc/runsc/config.toml"
'''
open(p, 'w').write(t.replace(anchor, anchor + block, 1))
print("    runsc handler INSERTED into template")
PY
fi

echo "==> staging complete. Still INERT: containerd has not restarted."
echo "    rendered runsc count (expect 0): $(grep -c runsc "$RENDERED" 2>/dev/null || echo 0)"
echo "==> next, from your workstation:  ./activate-gvisor-node.sh <node> [ssh-user] [kube-node-name]"
