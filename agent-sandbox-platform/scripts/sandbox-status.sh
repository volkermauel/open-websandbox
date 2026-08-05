#!/usr/bin/env bash
# sandbox-status.sh — one-glance health + usage snapshot of the agent-sandbox platform.
# Run from anywhere with kubectl pointed at the cluster.
# Usage: ./sandbox-status.sh [-w]   ( -w also tails recent broker reaper/error lines )
set -u
NS_RT="agent-sandbox-runtime"      # sandbox pods + claims + warm pool
NS_SYS="agent-sandbox-system"      # broker + controller + router

c() { printf '\033[1;36m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }

c "═══ agent-sandbox platform status  ($(date -u +%FT%TZ)) ═══"

# --- claims ------------------------------------------------------------------
c "▚ SandboxClaims ($NS_RT)"
n=$(kubectl get sandboxclaims -n "$NS_RT" -o name 2>/dev/null | wc -l)
kubectl get sandboxclaims -n "$NS_RT" \
  -o custom-columns=NAME:.metadata.name,AGE:.metadata.creationTimestamp 2>/dev/null \
  | awk 'NR==1{print} NR>1{print}' | column -t 2>/dev/null
dim "   total claims: $n  (owui-<hash> = ephemeral per-session, owui-p-<hash> = persistent per-user)"

# ephemeral claims grouped by real user (from broker logs)
echo; c "▚ Active users (last 1000 broker lines → user → claim count)"
kubectl logs -n "$NS_SYS" deploy/owui-broker --tail=1000 2>/dev/null \
  | grep -oE "user=[A-Za-z0-9_-]+ profile=[a-z]+ -> claim=[A-Za-z0-9_-]+" \
  | awk '{print $1}' | sort | uniq -c | sort -rn | head -10
dim "   >1 distinct claim per active user = session fragmentation (each = 1 pod)"

# --- warm pool ---------------------------------------------------------------
echo; c "▚ WarmPool (free pre-warmed sandboxes)"
kubectl get sandboxwarmpools -n "$NS_RT" \
  -o jsonpath='{range .items[*]}{.metadata.name}: replicas(free)={.status.replicas} ready={.status.readyReplicas}{"\n"}{end}' 2>/dev/null

# --- pods: runtime -----------------------------------------------------------
echo; c "▚ Sandbox pods ($NS_RT)"
kubectl get pods -n "$NS_RT" -o custom-columns=NAME:.metadata.name,NODE:.spec.nodeName,READY:.status.containerStatuses[0].ready,RESTARTS:.status.containerStatuses[0].restartCount 2>/dev/null | head -15
dim "   running: $(kubectl get pods -n "$NS_RT" --field-selector=status.phase=Running -o name 2>/dev/null | wc -l)  | by node: $(kubectl get pods -n "$NS_RT" -o jsonpath='{range .items[*]}{.spec.nodeName} {end}' 2>/dev/null | tr ' ' '\n' | sort | uniq -c | tr '\n' ' ')"

# --- pods: system ------------------------------------------------------------
echo; c "▚ Control plane ($NS_SYS)"
kubectl get pods -n "$NS_SYS" -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[0].ready,RESTARTS:.status.containerStatuses[0].restartCount 2>/dev/null

# --- quota -------------------------------------------------------------------
echo; c "▚ ResourceQuota ($NS_RT)"
kubectl describe resourcequota -n "$NS_RT" 2>/dev/null | awk '/^Used/,/^$/{print}' | head -12

# --- PVCs --------------------------------------------------------------------
echo; c "▚ Persistent volumes (parked sandbox data)"
kubectl get pvc -n "$NS_RT" -o custom-columns=NAME:.metadata.name,STATUS:.status.phase,CAP:.status.capacity.storage,SC:.spec.storageClassName 2>/dev/null | head -10

# --- node pressure -----------------------------------------------------------
echo; c "▚ Worker node pressure"
if kubectl top nodes >/dev/null 2>&1; then
  kubectl top nodes 2>/dev/null | grep -E "NAME|gvisor-worker-|gvisor-control-plane" || kubectl top nodes
else
  dim "   (metrics-server unavailable — checking conditions only)"
fi
for nd in $(kubectl get nodes -o name 2>/dev/null | grep -iE "w[0-9]" | sed 's|node/||'); do
  bad=$(kubectl describe node "$nd" 2>/dev/null | grep -E "MemoryPressure|DiskPressure|PIDPressure" | grep -vc "False")
  [ "$bad" -gt 0 ] && echo "   ⚠ $nd has pressure!" || true
done
dim "   (no ⚠ above = all workers clean)"

# --- optional: broker reap/error tail ---------------------------------------
if [ "${1:-}" = "-w" ] || [ "${2:-}" = "-w" ]; then
  echo; c "▚ Broker: reap / park / error events (last 2000 lines)"
  kubectl logs -n "$NS_SYS" deploy/owui-broker --tail=2000 2>/dev/null \
    | grep -iE "reap|park|suspend|operatingMode ->|error|traceback|exception|warn" | tail -20
  dim "   (empty = nothing reaped/errored recently — reaper acts at 30min idle)"
fi
echo
