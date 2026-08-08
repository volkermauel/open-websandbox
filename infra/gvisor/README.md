# gVisor on snap MicroK8s — node install playbook

Idempotent scripts to add the **gVisor (`runsc`)** containerd runtime to a
snap-packaged MicroK8s worker node, and to bring it online safely. Used to
enable gVisor on all three workers (`gvisor-worker-1/2/3`) of the lab cluster
Keep this playbook to extend the cluster (new nodes) or rebuild
after a node replacement.

This is the platform prerequisite for the AgentSandbox direction
(`AgentSandbox.md`): it satisfies §6.2 (dedicated-node gVisor requirements) and
§22 Phase 1 (runtime isolation). It maps to the future GitOps
`scripts/install-gvisor-check.sh` + `verify-runtime.sh` from §21.

## Why this works (online-safety model)

A `containerd` restart **does not kill running containers** — they are separate
`runc` processes; the kubelet reconnects and reconciles. MicroK8s does **not**
watch `containerd-template.toml` for changes, so editing the template is
**inert** until you restart the `snap.microk8s.daemon-containerd` service.
Verified live: zero pod disruption across worker restarts (stateful workloads
stayed Running).

Cordon first only to stop *new* pods scheduling during the ~20s reconcile
window; it has no effect on running pods.

## Prerequisites (per node)

- snap MicroK8s **classic** snap (unconfined) so `/usr/local/bin` is on
  containerd's PATH — the **strict** snap would confine it (different procedure).
- x86_64 (gVisor also publishes aarch64).
- No nested virtualization required: runsc uses the **systrap** platform by
  default. For the higher-performance **kvm** platform you need `/dev/kvm`
  (nested virt enabled on the hypervisor VM).
- The shipped MicroK8s template must still contain the `kata` handler block
  (the script anchors on `BinaryName = "kata-runtime"`). If a future MicroK8s
  drops it, anchor on the `runc`/`nvidia` block instead or add the handler by
  hand.
- Workstation with `kubectl` (cluster admin) and SSH access to the node.

## Files

| File | Runs | Purpose |
|---|---|---|
| `install-gvisor-node.sh` | **on the node** (root) | Stages binaries + edits the template. Inert. |
| `activate-gvisor-node.sh` | **workstation** | Cordon → restart → verify → probe → uncordon. |
| `manifests/runtimeclass-gvisor.yaml` | cluster | The `gvisor` RuntimeClass (Variant A simple / B dedicated+ tainted). |
| `manifests/gvisor-verify.yaml` | cluster | One-shot probe pod. |

## Usage

```bash
cd infra/gvisor

# 1. stage (inert) — pipe over SSH to the target node
ssh ubuntu@<node-ip> 'sudo bash -s' < install-gvisor-node.sh

# 2. activate (online-safe) — from your workstation
./activate-gvisor-node.sh <node-ip>            # ssh-user defaults to ubuntu
# or, if the kubectl node name differs from the SSH host:
./activate-gvisor-node.sh <node-ip> ubuntu <kube-node-name>
```

The activate script creates the `gvisor` RuntimeClass (if missing) from
`manifests/runtimeclass-gvisor.yaml`, runs a probe pod, and confirms gVisor
(`uname -r` → `4.19.0-gvisor`, `dmesg` → "Starting gVisor...").

### Pinning gVisor (reproducibility)

`install-gvisor-node.sh` fetches `release/latest` by default. Pin it:

```bash
ssh ubuntu@<node> 'sudo GVISOR_RELEASE=release-20260727.0 bash -s' < install-gvisor-node.sh
```

Record the resolved version (`runsc --version`) — currently `release-20260727.0`.

## ⚠ Stateful primary caveat

If the node hosts a **stateful primary** (e.g. a database), fail it over **before**
activating so no primary is at risk during the restart — follow that workload's
operator docs (e.g. promote a replica). Replicas surviving a restart is fine; a
primary bouncing mid-write is what you avoid.

## Manual probe (without the activate script)

```bash
kubectl apply -f manifests/runtimeclass-gvisor.yaml
kubectl apply -f manifests/gvisor-verify.yaml          # edit nodeSelector first
sleep 10
kubectl logs gvisor-verify
kubectl delete -f manifests/gvisor-verify.yaml
```

## Post-upgrade re-validation

After any MicroK8s or node upgrade, re-confirm gVisor (§25 runbook):

```bash
kubectl apply -f manifests/gvisor-verify.yaml   # per gVisor node
kubectl logs gvisor-verify | grep gvisor
```

If the rendered `containerd.toml` lost the handler (MicroK8s rewrote the
template on upgrade), re-run `install-gvisor-node.sh` then
`activate-gvisor-node.sh`.
