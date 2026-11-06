# chart/sandbox-template — desktop/browser sidecar

## ADDED Requirements

### Requirement: desktop sidecar in the sandbox pod template

When `desktop.enabled` is true, every SandboxTemplate rendered by the chart
SHALL contain a second container named `desktop` running the open-terminal
desktop/browser image on `127.0.0.1:8000`, restricted to the proven profile:
`capabilities.drop: ["ALL"]`, `allowPrivilegeEscalation: false`,
`runAsNonRoot: true`, uid/gid = `sandboxTemplate.uid` (1000), and
`seccompProfile.type: RuntimeDefault`.

#### Scenario: enabled by default

- **WHEN** the chart is rendered with default values
- **THEN** each rendered SandboxTemplate (base, warm-pool, s3-record profiles)
  contains the `desktop` container, a `desktop-shm` memory-backed emptyDir
  volume, and the corresponding mount

#### Scenario: sidecar image is independently pinnable

- **WHEN** an operator overrides `desktop.image`
- **THEN** the container renders that exact reference verbatim, including
  digest-pinned forms (`...@sha256:<digest>`)

### Requirement: runtime desktop env contract

When `desktop.enabled` is true, the runtime container SHALL receive
`DESKTOP_URL` (default `http://127.0.0.1:8000`) and `DESKTOP_API_KEY` env
vars, and the desktop container SHALL receive `OPEN_TERMINAL_API_KEY` with
the SAME value, rendered from one Helm pass.

#### Scenario: shared bearer token

- **WHEN** the chart is rendered (explicit `desktop.apiKey` or auto-generated)
- **THEN** `DESKTOP_API_KEY` on the runtime container equals
  `OPEN_TERMINAL_API_KEY` on the desktop container within the same rendered
  template

#### Scenario: disabled restores the previous pod

- **WHEN** `desktop.enabled` is false
- **THEN** no `desktop` container, no `desktop-shm` volume, and no
  `DESKTOP_URL`/`DESKTOP_API_KEY` env are rendered — the pod template matches
  the pre-sidecar rendering

### Requirement: browser shared memory volume

The desktop container SHALL mount a memory-backed emptyDir (`desktop-shm`)
at `/dev/shm` with `sizeLimit` = `desktop.shmSize` (default 512Mi).

#### Scenario: configurable shm size

- **WHEN** `desktop.shmSize` is overridden (e.g. `1Gi`)
- **THEN** the rendered emptyDir `sizeLimit` equals the override

### Requirement: KIND e2e profiles keep the desktop lane off

Every `values-kind*.yaml` e2e profile SHALL set `desktop.enabled: false`,
because the desktop image is pulled from a remote registry while KIND e2e
pins `imagePullPolicy: Never` and only uses node-loaded images.

#### Scenario: KIND render stays hermetic

- **WHEN** any e2e profile is rendered (`-f values-kind*.yaml`)
- **THEN** no sandbox pod template references the remote desktop image
