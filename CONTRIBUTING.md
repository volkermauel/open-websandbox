# Contributing to open-websandbox

Thanks for helping! open-websandbox is a Kubernetes sandbox platform backing Open WebUI's
"Open Terminal": a Rust/Axum **broker** + **runtime**, a Go **sandbox-router** (built
from upstream `kubernetes-sigs/agent-sandbox`), and a **Helm chart** for deployment.

## Repo layout

- `rust/` — Rust workspace (`shared/`, `broker/`, `runtime/`; Axum): the broker owns sandbox
  lifecycle; the runtime runs inside each sandbox pod
- `open-websandbox-platform/chart/` — the Helm chart (the deployment mechanism)
- `open-websandbox-platform/deploy/base/` — the source manifests the chart reproduces (synced to live)
- `infra/gvisor/` — gVisor (runsc) node setup playbooks
- `openspec/` — specs + change proposals (we plan with OpenSpec)
- `research/` — prior-art / design notes

## Development

- **Rust control plane.** Format, lint, and test the broker + runtime:

  ```bash
  cd rust
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test --workspace
  ```

  Tests are real-filesystem + PTY integration tests (Linux) — no cluster needed.
- **Helm chart:** keep it reproducing `deploy/base/` exactly (parameterized only by values):

  ```bash
  helm lint open-websandbox-platform/chart
  helm template open-websandbox-platform/chart >/dev/null
  ```

- **End-to-end (KIND):** see `tests/e2e/`. Both **runc** and **gVisor** run in KIND locally —
  gVisor uses the systrap platform (no `/dev/kvm` needed); see `scripts/setup-kind-gvisor.sh`
  for the gVisor cluster (its final step is a gVisor smoke pod).

## Planning & changes

Non-trivial work starts with an **OpenSpec** change proposal under `openspec/changes/` (see
the existing `adopt-agent-sandbox` and `release-v0-1-0` changes for the format). Open a PR;
CI runs `cargo fmt`/`clippy`/`test` on every push, and the KIND e2e suite on PRs.

## Commits & PRs

- Descriptive, conventional commit messages.
- **Keep manifests and chart in sync:** the Helm chart must reproduce `deploy/base/` exactly,
  except for the parameterized knobs (images, env, runtimeClassName, sizes, …).
- Don't change running behavior without a note in the PR description + (if user-facing) a
  `CHANGELOG.md` entry.

## License

open-websandbox is licensed under the **GNU Affero General Public License v3.0 only**
(`AGPL-3.0-only`); see [`LICENSE`](LICENSE). By contributing, you agree that your
contributions are licensed under the same terms.
