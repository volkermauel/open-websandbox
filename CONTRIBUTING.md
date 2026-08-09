# Contributing to open-websandbox

Thanks for helping! open-websandbox is a Kubernetes sandbox platform backing Open WebUI's
"Open Terminal": a Python/FastAPI **broker** + **runtime**, a Go **sandbox-router** (built
from upstream `kubernetes-sigs/agent-sandbox`), and a **Helm chart** for deployment.

## Repo layout

- `open-websandbox-platform/broker/` — broker (Python, FastAPI): owns sandbox lifecycle
- `open-websandbox-platform/runtime/` — runtime (Python, FastAPI): runs inside each sandbox pod
- `open-websandbox-platform/chart/` — the Helm chart (the deployment mechanism)
- `open-websandbox-platform/deploy/base/` — the source manifests the chart reproduces (synced to live)
- `infra/gvisor/` — gVisor (runsc) node setup playbooks
- `openspec/` — specs + change proposals (we plan with OpenSpec)
- `research/` — prior-art / design notes

## Development

- **Python 3.12.** Install test deps + the component requirements, then run unit tests:

  ```bash
  pip install -r requirements-test.txt \
              -r open-websandbox-platform/runtime/requirements-app.txt \
              -r open-websandbox-platform/runtime/requirements-common.txt \
              -r open-websandbox-platform/broker/requirements.txt
  pytest tests/unit -q
  ```

  Runtime tests use a **real filesystem + PTY** (Linux) — no cluster needed.
- **Lint:** `ruff check open-websandbox-platform`.
- **Helm chart:** keep it reproducing `deploy/base/` exactly (parameterized only by values):

  ```bash
  helm lint open-websandbox-platform/chart
  helm template open-websandbox-platform/chart >/dev/null
  ```

- **End-to-end (KIND):** see `tests/e2e/`. Runs on **runc** — gVisor/runsc cannot nest in
  KIND, so gVisor-specific checks are a separate manual smoke (`scripts/smoke-gvisor-sandbox.yaml`).

## Planning & changes

Non-trivial work starts with an **OpenSpec** change proposal under `openspec/changes/` (see
the existing `adopt-agent-sandbox` and `release-v0-1-0` changes for the format). Open a PR;
CI runs `ruff` + unit tests on every push, and the KIND e2e suite on PRs.

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
