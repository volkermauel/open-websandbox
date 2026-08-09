#!/usr/bin/env bash
# Bring up a gVisor-enabled KIND cluster locally for the open-websandbox e2e suite.
#
# gVisor delivery is identical to CI: runsc + its containerd shim + the gvisor-bin
# sidecar dir are installed on the HOST (infra/kind/install-runsc.sh) and bind-mounted
# into the KIND node via infra/kind/kind-config-gvisor.yaml. No runsc is baked into a
# node image, so a gVisor release layout change can't break the build (the failure mode
# of the old bake-into-image approach).
#
# The only local-only wrinkle: some docker daemons cap container soft nofile at 1024,
# which crashes kube-proxy/kindnet with "too many open files". On such hosts we use a
# tiny nofile-raising wrapper node image (infra/kind/Dockerfile.node-nofile). Set
# NODE_IMAGE=kindest/node:v1.31.0 to skip the wrapper (e.g. CI, or sane hosts).
set -Eeuo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-open-websandbox-e2e}"
NODE_IMAGE="${NODE_IMAGE:-kindest-node-nofile:v1.31.0}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
KIND_CFG="$ROOT/infra/kind/kind-config-gvisor.yaml"
CTX="kind-$CLUSTER_NAME"

for c in docker kind kubectl; do
  command -v "$c" >/dev/null 2>&1 || { echo "missing: $c" >&2; exit 1; }
done

# 1. Install runsc on the host (shared with CI).
echo "== installing runsc on host =="
bash "$ROOT/infra/kind/install-runsc.sh"

# 2. Build the nofile-raising node wrapper (local-only; CI uses plain kindest/node).
if ! docker image inspect "$NODE_IMAGE" >/dev/null 2>&1; then
  echo "== building local node image ($NODE_IMAGE) =="
  docker build -t "$NODE_IMAGE" "$ROOT/infra/kind/"
fi

# 3. (Re)create the cluster with the gVisor config + nofile node.
if kind get clusters 2>/dev/null | grep -Fxq "$CLUSTER_NAME"; then
  echo "cluster '$CLUSTER_NAME' exists; deleting first" >&2
  kind delete cluster --name "$CLUSTER_NAME"
fi
echo "== creating cluster =="
kind create cluster --name "$CLUSTER_NAME" --image "$NODE_IMAGE" --config "$KIND_CFG"
sleep 10

# 4. RuntimeClass + sanity checks.
kubectl --context "$CTX" apply -f "$ROOT/infra/kind/runtimeclass-gvisor.yaml"

echo "== runsc present in node =="
docker exec "$CLUSTER_NAME-control-plane" /usr/local/bin/runsc --version

echo "== kube-proxy healthy? =="
kubectl --context "$CTX" -n kube-system wait --for=condition=Ready pod -l k8s-app=kube-proxy \
  --timeout=120s || {
    echo "  kube-proxy not Ready; logs:" >&2
    kubectl --context "$CTX" -n kube-system logs -l k8s-app=kube-proxy --tail=5 >&2 || true
  }

# 5. gVisor smoke test: a pod that selects the gvisor RuntimeClass must go Ready.
echo "== gVisor smoke test =="
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
cat >"$TMP/gvisor-test.yaml" <<'KUBERNETES'
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
kubectl --context "$CTX" apply -f "$TMP/gvisor-test.yaml"
if ! kubectl --context "$CTX" wait --for=condition=Ready pod/gvisor-test --timeout=180s; then
  kubectl --context "$CTX" describe pod gvisor-test || true
  docker exec "$CLUSTER_NAME-control-plane" journalctl -u containerd --no-pager -n 80 || true
  exit 1
fi
echo
kubectl --context "$CTX" logs gvisor-test
echo
echo "== gVisor KIND cluster '$CLUSTER_NAME' ready =="
