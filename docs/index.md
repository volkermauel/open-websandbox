# open-websandbox

**A Kubernetes sandbox runtime that backs Open WebUI's "Open Terminal" feature.**

Each chat gets an isolated Linux sandbox running under **gVisor (`runsc`)**. An agent
(or a human in a terminal UI) can run shell commands, edit files, and install packages
on what looks like a throwaway VM — but without the VM's blast radius. One gVisor sandbox
runs **per active chat**; a warm pool hides cold-start latency; default-deny networking
keeps sandboxes off the rest of the cluster.

The control plane rests on the upstream
[`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
controller (pinned **v0.5.6**, manifest vendored + SHA256-recorded in this repo). It is
made of three components:

- **broker** (Rust/Axum) — the front door: authenticates Open WebUI, resolves or
  creates the sandbox for a user + session, and reverse-proxies requests to the runtime.
- **runtime** (Rust/Axum) — runs inside each sandbox pod: `POST /execute`,
  `GET|POST /files/*`, `GET /ports`, and interactive PTY terminals over WebSocket.
- **sandbox-router** (Go, self-built from upstream) — reverse-proxies traffic to the live
  sandbox pod IP, with a Pod-IP cache fast path.

> This page is a concise overview. For the GitHub-rendered project README, see the
> [repository root](https://github.com/volkermauel/open-websandbox#readme).

## Documentation

| Doc | What's in it |
|-----|--------------|
| [Architecture](architecture.md) | broker ↔ router ↔ runtime ↔ controller flow, per-chat lifecycle, ephemeral vs. persistent workspaces, isolation layers. |
| [Deployment guide](deploy.md) | Full install: gVisor nodes, upstream controller + CRDs, RWX storage, image build/load, broker shared-secret, Open WebUI wiring, Helm values reference. |
| [Operations runbook](operations.md) | Warm-pool tuning, idle park/reap policy, quotas, backup & restore, troubleshooting, upgrades. |
| [Security model](security.md) | Threat model, defense-in-depth isolation layers, residual risks. |
| [Production-readiness checklist](production-readiness-checklist.md) | Table-stakes vs. advanced checklist, benchmarked against peers. |

## Project status

Pre-release. Outstanding work and known risks are tracked in
[GitHub issues](https://github.com/volkermauel/open-websandbox/issues) (look for the
`roadmap` and `known-limitation` labels); see also the
[Production-readiness checklist](production-readiness-checklist.md).
