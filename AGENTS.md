# AGENTS.md

> Fast-path orientation for coding agents and humans working in this repo.
> Deeper detail lives in `docs/` (built into a Pages site via MkDocs) and `openspec/`.

## What is open-websandbox?

open-websandbox is a Kubernetes sandbox runtime that backs Open Web UI's "Open Terminal"
feature. Each chat gets an isolated Linux sandbox running under **gVisor (`runsc`)**; an
agent (or a human in a terminal UI) can run shell commands, edit files, and install packages
in what looks like a throwaway VM — but without the VM's blast radius. One gVisor sandbox
runs **per active chat**; a warm pool hides cold-start latency; default-deny networking
keeps sandboxes off the rest of the cluster. The control plane is a Rust/Axum
**broker** + **runtime** plus a Go **sandbox-router** (self-built from the upstream
[`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
controller, pinned **v0.5.3**), deployed via a **Helm chart**.

> **Naming note.** Project naming is unified to `open-websandbox` (issue #3). The
> platform directory is `open-websandbox-platform/`; the three images live at
> `ghcr.io/volkermauel/open-websandbox-{broker,runtime,router}`. The **vendored
> upstream `agent-sandbox` system is intentionally kept verbatim**: the namespaces
> `agent-sandbox-system` / `agent-sandbox-runtime`, the `agent-sandbox-controller`
> Deployment, the `sandbox-router` / `sandbox-router-svc` services, and the
> `agents.x-k8s.io` CRDs (`Sandbox`, `SandboxTemplate`, `SandboxWarmPool`,
> `SandboxClaim`) all retain their upstream names — that is the external dependency,
> not our project. Node-selection label domain: `sandbox.open-websandbox.dev/type=sandbox`.

## Repository layout

| Path | What's here |
|------|-------------|
| `rust/` | Rust workspace (`shared/`, `broker/`, `runtime/`; Axum). The **broker** is the front door — authenticates Open Web UI, owns sandbox lifecycle + idle reaper; the **runtime** runs inside each sandbox pod (`POST /execute`, `/files/*`, `/ports`, PTY terminals over WS). |
| `open-websandbox-platform/chart/` | Helm chart: `templates/`, `values.yaml`, `values.schema.json`, `values-kind.yaml` (KIND e2e), `values-kind-gvisor.yaml`. |
| `open-websandbox-platform/deploy/base/` | Base Kubernetes manifests the chart reproduces (kept byte-for-byte in sync). |
| `open-websandbox-platform/upstream/` | Vendored upstream agent-sandbox CRDs + controller manifest (v0.5.3) + `SHA256SUMS`. |
| `infra/gvisor/` | Online-safe gVisor (`runsc`) install/activate playbooks + `RuntimeClass` manifests. |
| `docs/` | Architecture, deployment, operations, and security docs. |
| `openspec/` | OpenSpec specs + change proposals (how we plan non-trivial work). |
| `rust/*/tests/`, `tests/e2e/` | Rust integration tests (no cluster needed) and Python/KIND end-to-end tests (runc + gVisor). |
| `scripts/` | Helper scripts, e.g. `setup-kind-gvisor.sh`. |
| `.github/workflows/` | `ci.yml` (e2e), `rust.yml` (cargo fmt/clippy/test/deny), `e2e.yml`, `release.yml`, `pages.yml`. |

## Build / test / lint

```bash
# The control plane (broker + runtime) is Rust. Format + lint + test:
cd rust
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace

# End-to-end tests are Python/KIND (both runc and gVisor run locally — see below):
pip install -r requirements-test.txt
pytest tests/e2e --collect-only -q   # full run needs a KIND cluster (R1)

# Helm chart checks
helm lint open-websandbox-platform/chart/
helm template open-websandbox open-websandbox-platform/chart/ -f open-websandbox-platform/chart/values-kind.yaml

# Docs site (source for GitHub Pages)
python -m venv /tmp/mkdocs-venv
/tmp/mkdocs-venv/bin/pip install -r requirements-docs.txt
/tmp/mkdocs-venv/bin/mkdocs build --strict
```

Keep `deploy/base/` and the chart reproducing the same manifests (parameterized only via
values). End-to-end tests live in `tests/e2e/` (KIND). Both `runc` and `gVisor` run in KIND
locally — `scripts/setup-kind-gvisor.sh` brings up a gVisor cluster (systrap needs no KVM).

## Rules (do not break these)

- **(R1) KIND must not touch the default kubeconfig.** Stand KIND up with its own
  kubeconfig path, and point every `kubectl`/`helm` invocation at it explicitly — either
  inline (`KUBECONFIG=/path/to/kind.kubeconfig kubectl …`) or once per shell
  (`export KUBECONFIG=/path/to/kind.kubeconfig`). **Never** let KIND overwrite
  `~/.kube/config`, and never rely on the ambient default kubeconfig in a KIND test. This
  protects your real cluster context and keeps the e2e run reproducible.
- **(R2) Test locally before you push — never push untested code.** Run the relevant checks
  above (`cargo fmt --check`, `cargo clippy --all-targets`, `cargo test --workspace` for the
  Rust control plane; `helm lint`/`helm template` for chart changes; `mkdocs build --strict`
  for docs changes) and confirm they pass before committing/pushing. CI verifies — it is not
  a first run.

## Key findings / gotchas (starter set)

Distilled from `docs/operations.md` — read it for full detail.

- **Persistent sandboxes need an RWX StorageClass.** `profile.persistentStorageClass` must
  point at a real `ReadWriteMany` class (e.g. CephFS). With only block/RWO storage, per-user
  PVCs stick `Pending` and park/resume cannot work.
- **The runtime namespace is default-deny, including cloud IMDS.** The runtime NetworkPolicy
  allows egress only to public internet (DNS + HTTP/80 + HTTPS/443); RFC1918 and link-local
  CIDRs are blocked — including `169.254.169.254` (cloud IMDS). Calls from inside a sandbox
  to internal services are *expected* to fail; that is the isolation working as designed.
- **Set `broker.sharedSecret` — never ship the default.** `BROKER_SHARED_SECRET` must be a
  fresh 32-byte secret (`openssl rand -hex 32`) set via `broker.sharedSecret` at install.
  Never deploy with the `dev-shared-secret-change-me` default. The broker **fails closed**
  (at boot and per-request) if the secret is unset or still the placeholder.
- **Leader election is required before `broker.replicas > 1`.** A single
  `coordination.k8s.io` `Lease` ensures only the elected broker runs the idle reaper. Without
  it, multiple replicas cause migration races and a reaper thundering-herd. `replicas: 1`
  (the default) is always safe; do not raise it on a build that lacks the lease.
- **gVisor `runsc` (systrap) runs in KIND without KVM — don't fall back to `runc` unnecessarily.**
  `runsc` uses the **systrap** platform (a userspace syscall interceptor) by default; systrap needs
  **no `/dev/kvm` or nested virtualization**, so it runs inside KIND on any Linux host — including
  VMs without nested virt (verified: the full e2e suite passes under gVisor on a KVM-guest dev box
  with no `/dev/kvm`; run `scripts/setup-kind-gvisor.sh`). Only the opt-in **`kvm`** platform
  (`RUNSC_PLATFORM=kvm`) requires `/dev/kvm`. Production must always run under `gvisor`; `runc`
  (`runtimeClassName: ""`) is fine only when you deliberately want the faster, weaker-isolation path.

## License

open-websandbox is licensed under the **GNU Affero General Public License v3.0 only**
(`AGPL-3.0-only`); see [`LICENSE`](LICENSE). By contributing you agree your contributions
are licensed under the same terms — see [`CONTRIBUTING.md`](CONTRIBUTING.md).
