# Proposal: desktop/browser sidecar in the sandbox pod (chart)

## Why

Exactly ONE Open WebUI terminal connection should exist: the open-websandbox
broker → runtime path. That connection must ALSO serve the desktop/browser
tabs, so the separate `open-terminal-k8s-proxy` (pod-per-session) lane can be
removed once the sandbox path passes verification.

A spike (2026-11-05, ns `agent-sandbox-runtime`, restricted PSA) proved a
second container in the sandbox pod —
`ghcr.io/volkermauel/open-terminal:0.16.1-desktop@sha256:626553d…` — serves
`:8000` with `desktop: true` in `/api/config` and a VNC desktop at `/desktop`,
provided it runs the restricted profile (capabilities drop ALL, no privilege
escalation, uid/gid 1000, RuntimeDefault seccomp) with a memory-backed
`/dev/shm`. The stock image entrypoint breaks under `drop: ALL` (setcap
indirection); a `0.16.1-desktop-sandbox` image variant with a plain-python3
entrypoint is the long-term image.

## What Changes

- Chart only (`open-websandbox-platform/chart`): a `desktop.*` values block
  (enabled default true) renders a second `desktop` container into EVERY
  SandboxTemplate profile (base, warm-pool, s3-record) plus a memory-backed
  `desktop-shm` emptyDir at `/dev/shm`.
- The runtime container gains `DESKTOP_URL` + `DESKTOP_API_KEY` when the
  sidecar is enabled; both are omitted when disabled, so the pod renders
  byte-identical to the pre-sidecar template.
- `desktop.image` is a full image reference (external image — NOT the
  `imageRegistry`/`imageOwner`/`imageTag` platform trio), digest-pinnable.
- `desktop.apiKey` ("": derived deterministically from the broker shared
  secret, stable across renders/upgrades) is the shared
  runtime↔sidecar bearer token, rendered into both containers from one Helm
  pass so they always agree.
- KIND e2e profiles (`values-kind*.yaml`) set `desktop.enabled: false` — the
  ghcr.io image is not node-loaded there and KIND pins `imagePullPolicy:
  Never`.
- Rust code is NOT touched here (the runtime's desktop fallback/proxy
  activation via those env names is a separate change).

## Impact

- Affected specs: chart/sandbox-template
- No change to broker/runtime images or the live cluster from this change
  alone; the live template picks the sidecar up on the next chart deploy.
