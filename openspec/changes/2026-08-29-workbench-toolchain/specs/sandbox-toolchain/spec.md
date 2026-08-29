# sandbox-toolchain

## ADDED Requirements

### Requirement: curated toolchain manifest as the image source of truth

The runtime image toolchain SHALL be driven by a single manifest
(`rust/runtime/tools.json`) that the image build expands into package
installs, so the manifest and the image contents cannot drift.

#### Scenario: manifest drives the build

- **WHEN** the runtime image is built
- **THEN** every apt/pip package listed in `tools.json` is installed from
  that file (via `gen-manifest.py --print-apt/--print-pip`) and
  `gen-manifest.py --self-test` validates the manifest with stdlib json only

#### Scenario: pinned tarball tools

- **WHEN** the image installs tools not available (or EOL) in bookworm apt
- **THEN** Node 22 LTS, PowerShell 7, .NET SDK 8, the CFR decompiler jar,
  and the duckdb CLI are fetched as tarballs/artifacts pinned by VERSION and
  SHA256 under `/opt`, with PATH/`DOTNET_ROOT`/`PSModulePath` wired up, and
  `wix` is installed only if `dotnet tool install wix` succeeds on linux
  (in practice skipped — Windows-only; Debian ships msitools without `wixl`,
  so PSAppDeployToolkit + nsis + msitools are the documented MSI-adjacent paths)

### Requirement: apt-only sudo whitelist

The runtime image SHALL grant the sandbox user passwordless sudo for
`apt-get` verbs only, and nothing else.

#### Scenario: apt allowed, everything else denied

- **WHEN** the sandbox user runs `sudo apt-get update && sudo apt-get
  install -y <pkg>`
- **THEN** it succeeds and the package is installed into the ephemeral
  per-pod rootfs
- **WHEN** the sandbox user runs any other sudo command (e.g. `sudo rm`)
- **THEN** it is denied with a non-zero exit and a "not allowed" message

#### Scenario: PEP-668 relief

- **WHEN** the sandbox user runs `pip install --user <pkg>` or
  `pip install --target /packages/py <pkg>`
- **THEN** it succeeds (the image removes Debian's `EXTERNALLY-MANAGED`
  marker); no pip/npm prefix or cache configuration is forced by the image

### Requirement: capability manifest in the system prompt

The runtime SHALL expose the baked-in toolchain inventory to the model via
`GET /system` behind the `SANDBOX_TOOLS_MANIFEST` knob, preserving the
upstream-verbatim prompt when the knob is off.

#### Scenario: knob off preserves upstream byte-for-byte

- **WHEN** `SANDBOX_TOOLS_MANIFEST` is unset/empty or points at a missing
  file
- **THEN** the `/system` prompt is exactly the stage-2 prompt (upstream
  v0.12.3 verbatim) — the pinned unit test passes unchanged

#### Scenario: toolchain section appended

- **WHEN** the knob is set to a readable manifest file
- **THEN** the prompt gains a `## Available toolchain (base image)` section
  containing the file's content after template expansion, for the default
  AND the operator-overridden prompt alike
- **AND** the manifest file itself is a pure, path-free inventory (per-area
  name—version lines, build-time dpkg base count, live-state hint)

#### Scenario: workspace conventions are config-driven

- **WHEN** the knob is enabled and the workspace root is configured (env
  `WORKDIR`, default `/workspace`)
- **THEN** the prompt gains a `## Workspace conventions` section after the
  toolchain section, built from the configured workspace root: scratch
  files in `{workdir}/tmp` (created if missing), deliverables at the root,
  `/tmp` tmpfs vs persistent workspace, and the install recipes (persistent
  venv `{workdir}/.venv`, `pip --target /packages/py` + PYTHONPATH,
  npm prefix `/packages/npm`, sudo apt writes the ephemeral rootfs)
- **AND** with a non-default `WORKDIR` (e.g. `/data/ws`) the prompt contains
  `/data/ws/tmp` and never the string `/workspace/tmp` — no hardcoded
  workspace path exists in the section-building code

#### Scenario: chart toggle

- **WHEN** the chart is installed with `sandboxTemplate.toolsManifest=true`
  (default)
- **THEN** sandbox pods receive `SANDBOX_TOOLS_MANIFEST` pointing at the
  image's capability manifest; `false` renders the env var explicitly empty

### Requirement: sandbox-tools live inventory command

The image SHALL ship `/usr/local/bin/sandbox-tools` printing the baked
manifest plus a live delta.

#### Scenario: live delta

- **WHEN** `sandbox-tools` runs in a sandbox
- **THEN** it exits 0, prints the baked manifest, re-probes key tool
  versions, compares the current `dpkg -l` count against the build-time base
  count baked into the manifest, and prints the workspace conventions
  rendered from `${WORKDIR:-/workspace}`
