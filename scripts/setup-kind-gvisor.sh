#!/usr/bin/env bash
# Build a gVisor-enabled KIND node image and bring up a local cluster for the
# open-sandbox e2e suite. gVisor cannot run on the host and be selected per-pod;
# runsc + its containerd shim must live INSIDE the node image, and the node's
# containerd must register the "runsc" runtime. This script does both.
#
# Two host-specific adaptations (no host changes required):
#   1. Build on kindest/node:v1.31.0 (cached) → containerd 1.7.x, so the containerd
#      config patch uses the 1.x plugin path ("io.containerd.grpc.v1.cri").
#   2. Raise soft nofile at the node's PID 1 (the entrypoint, before systemd). The host
#      docker daemon sets container soft nofile=1024 (hard=524288); kube-proxy + pods
#      inherit that and crash with "too many open files". Raising the soft limit on PID 1
#      (then exec'ing systemd) propagates 524288 to systemd, containerd, kubelet, and every
#      pod — without a docker restart or touching the host.
set -Eeuo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-open-sandbox-e2e}"
KIND_NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.31.0}"
CUSTOM_NODE_IMAGE="${CUSTOM_NODE_IMAGE:-kindest-node-gvisor:v1.31.0}"

for c in docker kind kubectl; do
  command -v "$c" >/dev/null 2>&1 || { echo "missing: $c" >&2; exit 1; }
done
docker info >/dev/null

if kind get clusters 2>/dev/null | grep -Fxq "$CLUSTER_NAME"; then
  echo "cluster '$CLUSTER_NAME' exists; deleting first" >&2
  kind delete cluster --name "$CLUSTER_NAME"
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Wrapper that raises the soft nofile limit on PID 1, then hands off to the original
# KIND entrypoint. Quoted heredoc so "$@" survives literally into the file.
cat >"$WORKDIR/kind-entrypoint.sh" <<'WRAPPER'
#!/bin/sh
# Raise the soft nofile to the hard limit so systemd → containerd → kubelet → every pod
# inherits a sane fd ceiling. Idempotent: a no-op when soft already equals hard (sane hosts).
# NOTE: if kube-proxy still crashes with "too many open files", the host docker daemon caps
# container soft nofile at 1024 — set daemon.json default-ulimits.nofile=1048576 + restart docker.
ulimit -Sn "$(ulimit -Hn)" 2>/dev/null || true
exec /usr/local/bin/entrypoint "$@"
WRAPPER
chmod +x "$WORKDIR/kind-entrypoint.sh"

# --- gVisor node image (build context = $WORKDIR, so the wrapper is COPYable) ---
cat >"$WORKDIR/Dockerfile" <<DOCKERFILE
ARG KIND_NODE_IMAGE
FROM debian:bookworm-slim AS gvisor-download
RUN apt-get update && apt-get install -y --no-install-recommends bzip2 ca-certificates coreutils curl tar \
    && rm -rf /var/lib/apt/lists/*
RUN set -eux; \
    case "\$(dpkg --print-architecture)" in \
      amd64) A=x86_64 ;; arm64) A=aarch64 ;; *) echo "unsupported arch" >&2; exit 1 ;; \
    esac; \
    URL="https://storage.googleapis.com/gvisor/releases/release/latest/\${A}"; \
    mkdir -p /download /out; cd /download; \
    curl -fsSLO "\${URL}/gvisor.tar.bz2"; \
    curl -fsSLO "\${URL}/gvisor.tar.bz2.sha512"; \
    sha512sum -c gvisor.tar.bz2.sha512; \
    tar -xjf gvisor.tar.bz2 -C /out
FROM ${KIND_NODE_IMAGE}
COPY --from=gvisor-download /out/ /usr/local/bin/
RUN chmod a+rx /usr/local/bin/runsc /usr/local/bin/containerd-shim-runsc-v1 \
    && chmod -R a+rX /usr/local/bin/gvisor-bin \
    && /usr/local/bin/runsc --version
# Raise soft nofile for systemd services too (kubelet, containerd).
RUN mkdir -p /etc/systemd/system.conf.d \
    && printf '[Manager]\nDefaultLimitNOFILE=1048576\n' > /etc/systemd/system.conf.d/10-nofile.conf
# Install the PID-1 wrapper so ALL processes (incl. pods) inherit the raised soft limit.
COPY kind-entrypoint.sh /usr/local/bin/kind-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/kind-entrypoint.sh", "/sbin/init"]
DOCKERFILE

# --- cluster config: register runsc (containerd 1.x plugin path) ---
cat >"$WORKDIR/kind.yaml" <<KINDCONFIG
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runsc]
      runtime_type = "io.containerd.runsc.v1"
nodes:
  - role: control-plane
KINDCONFIG

# --- gVisor smoke test: RuntimeClass + a pod that selects it ---
cat >"$WORKDIR/gvisor-test.yaml" <<'KUBERNETES'
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: gvisor
handler: runsc
---
apiVersion: v1
kind: Pod
metadata:
  name: gvisor-test
spec:
  runtimeClassName: gvisor
  restartPolicy: Never
  containers:
    - name: test
      image: busybox:1.37
      command: ["sh", "-c", "uname -a; echo '--- gVisor dmesg ---'; dmesg | head -n 6; sleep 3600"]
KUBERNETES

echo "== building gVisor node image (runsc + nofile wrapper) =="
docker build --build-arg "KIND_NODE_IMAGE=$KIND_NODE_IMAGE" -t "$CUSTOM_NODE_IMAGE" "$WORKDIR"

echo "== creating cluster =="
kind create cluster --name "$CLUSTER_NAME" --image "$CUSTOM_NODE_IMAGE" --config "$WORKDIR/kind.yaml"
# Let the DaemonSets (kube-proxy/kindnet) + default ServiceAccount materialise before verifying.
sleep 35

CTX="kind-$CLUSTER_NAME"
echo "== PID-1 + kube-proxy soft nofile (want >>1024) =="
docker exec "$CLUSTER_NAME-control-plane" sh -c '
  echo "  PID1:   $(grep "open files" /proc/1/limits | awk "{print \$4\"/\"\$5}")"
  p=$(pgrep -f kube-proxy | head -1)
  [ -n "$p" ] && echo "  kube-proxy (pid $p): $(grep "open files" /proc/$p/limits | awk "{print \$4\"/\"\$5}")" || echo "  kube-proxy: not running yet"
'
echo "== runsc present in node =="
docker exec "$CLUSTER_NAME-control-plane" /usr/local/bin/runsc --version
echo "== kube-proxy healthy? =="
kubectl --context "$CTX" -n kube-system wait --for=condition=Ready pod -l k8s-app=kube-proxy --timeout=120s || \
  { echo "  kube-proxy not Ready; logs:"; kubectl --context "$CTX" -n kube-system logs -l k8s-app=kube-proxy --tail=5; }

echo "== gVisor smoke test =="
kubectl --context "$CTX" apply -f "$WORKDIR/gvisor-test.yaml"
if ! kubectl --context "$CTX" wait --for=condition=Ready pod/gvisor-test --timeout=180s; then
  kubectl --context "$CTX" describe pod gvisor-test || true
  docker exec "$CLUSTER_NAME-control-plane" journalctl -u containerd --no-pager -n 80 || true
  exit 1
fi
echo
kubectl --context "$CTX" logs gvisor-test
echo
echo "== gVisor KIND cluster '$CLUSTER_NAME' ready =="
