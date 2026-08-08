#!/usr/bin/env bash
#
# activate-gvisor-node.sh — bring gVisor online on a snap MicroK8s node, safely.
# Assumes install-gvisor-node.sh already staged binaries + the template edit.
#
# Sequence: cordon -> restart containerd -> verify runsc handler loaded ->
#           verify pods survived -> probe RuntimeClass -> uncordon.
#
#   Online-safe model: a containerd restart does NOT kill running containers
#   (they are separate runc processes; kubelet reconciles). Verified live on
#   this cluster with zero pod disruption across worker restarts.
#
# !!! STATEFUL PRIMARY CAVEAT !!!
#   If the node hosts a stateful *primary* (e.g. a database), fail it over FIRST
#   so there is no primary at risk during the restart (follow its operator docs,
#   e.g. promote a replica). Replicas surviving a restart is fine; a primary
#   bouncing mid-write is what you avoid.
#
# Usage: ./activate-gvisor-node.sh <node-ip|hostname> [ssh-user] [kube-node-name]
#   <node-ip|hostname>  SSH target (default ssh user: ubuntu)
#   [kube-node-name]    kubectl node name if it differs from the SSH host
#
set -euo pipefail

SSH_TARGET="${1:?usage: $0 <node-ip|hostname> [ssh-user] [kube-node-name]}"
SSH_USER="${2:-ubuntu}"
KUBE_NODE="${3:-$SSH_TARGET}"
SVC=snap.microk8s.daemon-containerd
RENDERED=/var/snap/microk8s/current/args/containerd.toml
SSH=(ssh -o BatchMode=yes "$SSH_USER@$SSH_TARGET")

echo "==> [1/6] cordon $KUBE_NODE (blocks NEW scheduling; running pods unaffected)"
kubectl cordon "$KUBE_NODE"

echo "==> [2/6] restart $SVC on $SSH_USER@$SSH_TARGET"
"${SSH[@]}" "sudo systemctl restart $SVC"
for _ in $(seq 1 40); do
  s=$("${SSH[@]}" "systemctl is-active $SVC" 2>/dev/null || true)
  [ "$s" = active ] && break; sleep 1
done
echo "    containerd: $s  (started $("${SSH[@]}" "systemctl show $SVC -p ActiveEnterTimestamp --value"))"
echo "    letting kubelet reconcile (18s)..."; sleep 18
bad=$(kubectl get pods -A --field-selector spec.nodeName="$KUBE_NODE" --no-headers 2>/dev/null | grep -vE 'Running|Completed' || true)
if [ -n "$bad" ]; then
  echo "    non-Running pods (confirm these are pre-existing, e.g. old failed Jobs):"
  echo "$bad" | head | sed 's/^/      /'
else
  echo "    all pods Running/Completed"
fi

echo "==> [3/6] confirm rendered config now carries the runsc handler"
n=$("${SSH[@]}" "grep -c runsc $RENDERED 2>/dev/null || echo 0")
if [ "${n:-0}" -lt 1 ]; then
  echo "    !! runsc handler NOT in rendered config (count=$n). Leaving node cordoned." >&2
  exit 1
fi
echo "    runsc handler present (count=$n)"

echo "==> [4/6] ensure RuntimeClass 'gvisor' exists cluster-wide"
if ! kubectl get runtimeclass gvisor >/dev/null 2>&1; then
  kubectl apply -f manifests/runtimeclass-gvisor.yaml
fi
kubectl get runtimeclass gvisor | sed 's/^/    /'

echo "==> [5/6] probe: run a pod under gVisor pinned to $KUBE_NODE"
kubectl delete pod gvisor-verify --ignore-not-found >/dev/null 2>&1
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: gvisor-verify
spec:
  nodeSelector: {kubernetes.io/hostname: "$KUBE_NODE"}
  runtimeClassName: gvisor
  restartPolicy: Never
  containers:
  - name: t
    image: busybox:1.36
    command: ["sh","-c","echo uname=\$(uname -r); echo banner=\$(dmesg 2>/dev/null | grep -i gvisor | head -1); sleep 8"]
EOF
for _ in $(seq 1 40); do
  ph=$(kubectl get pod gvisor-verify -o jsonpath='{.status.phase}' 2>/dev/null || true)
  { [ "$ph" = Running ] || [ "$ph" = Succeeded ]; } && { sleep 3; break; }
  [ "$ph" = Failed ] && break
  sleep 1
done
out=$(kubectl logs gvisor-verify 2>/dev/null || true)
echo "    ${out:-(no output, phase=$ph)}"
kubectl delete pod gvisor-verify --ignore-not-found >/dev/null 2>&1
case "$out" in
  *gvisor*) echo "    gVisor CONFIRMED on $KUBE_NODE" ;;
  *) echo "    !! probe did not confirm gVisor (phase=$ph). Leaving node cordoned." >&2; exit 1 ;;
esac

echo "==> [6/6] uncordon $KUBE_NODE"
kubectl uncordon "$KUBE_NODE"
echo "==> done. gVisor active on $KUBE_NODE."
