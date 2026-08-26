# open-websandbox

A portable, isolated, multi-tenant **sandbox runtime** for Open WebUI's **Open Terminal**
integration — built on **kubernetes-sigs/agent-sandbox** and **gVisor**, with zero
proprietary runtime dependencies.

## What this project is

A Kubernetes platform that gives each Open WebUI user (and each chat) an isolated,
on-demand Linux sandbox exposing the **open-terminal REST/OpenAPI contract** Open WebUI
already speaks (`/execute`, `/files/*`, `/api/config`, `/openapi.json`, and an interactive
WebSocket PTY terminal at `/api/terminals/{id}`). Three components:

- **broker** (Python, FastAPI) — owns sandbox lifecycle: maps an OWUI session
  (`X-Session-Id` / user) to a per-user or per-chat sandbox via the agent-sandbox CRDs
  (SandboxClaim / SandboxTemplate / SandboxWarmPool), runs the warm pool, the 120s
  idle-suspend/reap policy, and the persistent-workspace PVCs.
- **runtime** (Python, FastAPI) — runs **inside** each sandbox pod, under gVisor: command
  execution (`asyncio` subprocess), a file API, and an interactive WebSocket PTY terminal.
- **sandbox-router** (Go, built from upstream agent-sandbox) — authenticates + reverse-
  proxies HTTP/WebSocket to sandbox pods.

It depends on the external **kubernetes-sigs/agent-sandbox** controller (v0.5.3) + its
CRDs. Sandboxes run under the **gVisor** (runsc) RuntimeClass.

## Isolation stance

- **Per-user** isolation (must) and **per-chat** isolation (achieved — one sandbox per
  chat, keyed `sha256(user_id/session_id)[:12]`).
- Layers: gVisor runsc, uid 1000, no service-account token, restricted PodSecurity,
  NetworkPolicy egress (DNS to public resolvers + HTTPS 443/80 to the public internet;
  private + cluster CIDRs blocked), persistent workspaces on a RWX StorageClass, tmpfs
  /tmp hard cap, per-uid PID cap.
- **Accepted residual risk:** shared kernel. The goal is "prevent accidental cross-session
  leakage + strong practical sandboxing" for trusted internal (Entra OIDC) users — not
  resistance to a dedicated hostile attacker.

## Key documents

- `openspec/changes/adopt-agent-sandbox/proposal.md` — what & why (current design)
- `openspec/changes/adopt-agent-sandbox/design.md` — architecture & decision log (D1–D14)
- `openspec/changes/adopt-agent-sandbox/specs/` — capability specs
- `openspec/changes/adopt-agent-sandbox/tasks.md` — phased implementation (Phases 0–4 done; 5–6 release-prep)
- `infra/gvisor/` — gVisor node setup playbooks

## Status

Platform built + proven on prod (ephemeral + persistent profiles, interactive terminal,
file API, security hardening: PID / env / DNS / egress / tmp caps). Currently in
release-prep: tests (unit + KIND e2e), docs, Kustomize packaging, CI, and router
self-build — toward **v0.1.0**.
