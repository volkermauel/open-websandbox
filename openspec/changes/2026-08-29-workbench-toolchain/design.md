# Design: workbench toolchain image + capability manifest

## Single fat image vs. profiles vs. on-demand

**Decision: one fat workbench image (~4 GB) for every sandbox.**

- *Profile images* (lean/office/workbench per tenant) split the tested
  surface 3-ways: every e2e matrix leg (runc/gvisor × 4 lanes) would need a
  per-profile build + `kind load`, tripling CI time, and the chart gains a
  repository-per-profile knob operators must keep consistent with the
  SandboxTemplate. The 4 GB pull is amortized per node (imageLayers are
  shared; only the first pod on a node pays), and cold starts are already
  dominated by PVC bind + gVisor pod start in the observed lanes.
- *On-demand install* (start lean, let the model `sudo apt-get` everything)
  burns 1-5 minutes of tenant wall-clock per chat on `apt-get update` +
  installs against ephemeral storage, repeated every pod (rootfs does not
  persist), and requires the sudo surface anyway. Baking the 90% case and
  sudo-ing the 10% tail is strictly better latency and strictly smaller
  surprise.

## sudo-apt-only vs. root vs. an /install API

**Decision: passwordless sudo restricted to `apt-get` verbs only, nothing
else.**

- *Full root* would let a tenant (or a prompt-injected model) mutate the
  rootfs arbitrarily — write cron-ish persistence into image-local paths,
  chmod setuid binaries, or pivot into the pod's other mounts. gVisor
  contains the blast radius (kernel-wise) but the rootfs is shared across
  the pod lifetime and reappears on restart only because it is rebuilt —
  root would make in-pod persistence trivial.
- *A broker-side `/install` API* re-invents apt with a queue, progress, and
  auth surface, and still ends up shelling out to apt inside the sandbox.
- The chosen `/etc/sudoers.d/sandbox` (mode 440, `visudo -c`-validated)
  allows exactly `apt-get update|install|remove|purge|upgrade|
  full-upgrade|clean|autoremove` (+ `Defaults logfile="/var/log/sudo.log"`)
  — package maintainer scripts still run as root inside gVisor, but that is
  the same trust we already extend to the image build itself, the rootfs is
  ephemeral per pod, and everything outside the apt verbs (rm, tee, chmod,
  su) stays unprivileged. Residual risk (malicious .deb maintainer script
  from a third-party repo a tenant adds) documented in `docs/security.md`.

## Direct internet vs. mirrors

**Decision: keep direct public-internet apt/pip/npm egress.** The runtime
NetworkPolicy already allows exactly DNS/80/443 to public space; adding an
in-cluster mirror would punch a hole into the default-deny namespace
isolation the policy exists to enforce, and mirrors are deployment-specific
(operators with an internal mirror can point sources.list at it themselves).
No `pip config`/`.npmrc`/prefix-cache env forcing is baked — tenants get
plain upstream behavior plus taught recipes.

## Build-time curated manifest vs. runtime dpkg scan

**Decision: `tools.json` is generated-consumed at BUILD time; the runtime
never inventories dpkg on the fly for the prompt.**

- A runtime `dpkg -l` scan appended to `/system` would inject 400+ package
  lines (thousands of tokens) into every model conversation — pure token
  bloat. The curated manifest is ~80 lines / ≤500 tokens and says *what the
  model can do*, not Debian's full dependency closure.
- tools.json is the single source the Dockerfile expands
  (`gen-manifest.py --print-apt/--print-pip`), so manifest and image cannot
  drift; the same file feeds the human docs table (by reference, versions
  probed at build).
- Live state is deliberately NOT in the prompt: `sandbox-tools` re-probes at
  run time and prints the baked base dpkg count vs `dpkg -l` now, and the
  manifest's final line tells the model live state may differ. This keeps
  `/system` deterministic (cacheable, testable) while still warning against
  staleness.

## Node/PowerShell/dotnet via pinned tarballs, not apt

Bookworm's nodejs is 18 (EOL upstream) — Node 22 LTS comes from the official
tarball into `/opt/node` with VERSION+SHA256 pinned, PATH via ENV, corepack
activating pnpm (+ yarn if `corepack prepare` succeeds; failure documented,
pnpm alone is fine). PowerShell 7 (`/opt/powershell`), .NET SDK 8
(`/opt/dotnet`, `DOTNET_ROOT`), CFR jar (`/opt/tools/cfr.jar` + `cfr`
wrapper), and the duckdb CLI (`/opt/tools`) follow the same pin+sha256
pattern. dotnet tools (`ilspycmd` pinned; `wix` only if
`dotnet tool install wix` succeeds on linux during the build — in practice
skipped, Windows-only warning; and Debian ships msitools WITHOUT `wixl` at
all, so PSADT + nsis + msitools' msiextract/msibuild are the documented
Windows-packaging paths) install into
`/opt/dotnet-tools` with `DOTNET_CLI_HOME=/opt`. PSAppDeployToolkit goes to
`/opt/psmodules` (+ `PSModulePath`). unrar needs the `non-free` component
appended to debian.sources (freeware license, NOTICE.md).

## Capability manifest + workspace conventions in `/system`

- `gen-manifest.py` output is a **pure, path-free toolchain inventory**:
  per-area `name — version` lines (probes tolerant of absence), a
  build-time dpkg base count in the footer, and the `sandbox-tools` hint as
  the final line. No workspace paths belong in it — the file is static at
  build time while `WORKDIR` is a per-deployment config.
- The **workspace conventions** section is therefore built in Rust
  (`system.rs`), config-driven, from the *same* `RuntimeConfig::workdir`
  the runtime uses for workspace resolution — one source of truth; if the
  config is somehow unset the config default (`/workspace` constant in
  `config.rs`) applies, never a literal inside the string-building code.
  Unit test pins this: a non-default `WORKDIR=/data/ws` renders
  `/data/ws/tmp` and the string `/workspace/tmp` must NOT appear.
- Both sections append AFTER template expansion (default or
  operator-overridden prompt alike), gated on the single knob
  `SANDBOX_TOOLS_MANIFEST` (path; empty/unset = disabled, missing file =
  warn + skip) so the upstream-verbatim byte-for-byte pin stays intact when
  disabled — the existing pinned unit test keeps passing unchanged.
- `sandbox-tools` mirrors both halves dynamically: prints the manifest file,
  then a live delta (re-probed versions, dpkg count vs baked base) and the
  conventions rendered from `${WORKDIR:-/workspace}`.

## Chart surface

`sandboxTemplate.toolsManifest` (bool, default `true`). When true the
SandboxTemplate env sets `SANDBOX_TOOLS_MANIFEST=/usr/local/share/sandbox-capabilities.md`;
when false it sets the env to `""` explicitly (the image's ENV would
otherwise keep it enabled — same scrub pattern the chart uses for
`KUBERNETES_*`). Schema updated; `helm lint` + `helm template` gate it.

## Testing

- **Unit** (`config.rs`/`system.rs`, existing naming style): knob default
  disabled; env parse; prompt byte-for-byte upstream with knob unset AND
  with a missing manifest path; tmpfile manifest ⇒ toolchain section with
  the file's content; conventions section present with the knob on and
  built from the configured `WORKDIR` (`/data/ws` case: contains
  `/data/ws/tmp`, not `/workspace/tmp`); append also applies to the
  operator-overridden template prompt; conventions absent when knob off.
- **Standalone**: `gen-manifest.py --self-test` validates tools.json parses
  and renders a manifest with the required structure (pure stdlib, no
  cluster); invoked from `tests/e2e/test_toolchain.py` as a no-broker test
  so it runs in the e2e CI lane, and runnable directly locally.
- **e2e** (`tests/e2e/test_toolchain.py`, broker-relay style): `/system`
  contains `Available toolchain` + `pandas` + the `/workspace/tmp`
  conventions line; `sandbox-tools` exit 0 + conventions line; `sudo
  apt-get install -y figlet` succeeds and `figlet` runs; `sudo rm /tmp/x`
  denied (non-zero, "not allowed"); `pip install --target /packages/py` of
  a tiny package + `PYTHONPATH=/packages/py python3 -c "import …"` works.
- **Gates**: cargo fmt/clippy/test, helm lint + template, `openspec
  validate`, `mkdocs build --strict`, `pytest tests/e2e --collect-only -q`,
  local `docker build` mirroring e2e.yml's context (`rust/`) + flags, and a
  bounded smoke run of the built image.
