# Proposal: workbench toolchain image + capability manifest

## Why

The runtime image today is a document-conversion image (bookworm-slim +
libreoffice-nogui). Models given a sandbox have to `apt-get` (impossible — no
sudo, read-only-ish flow) or pip-install the most basic tooling before doing
any real work: no git, no jq, no archives beyond tar/zstd, no Python, no R,
no Node, no pandoc/OCR depth, no Windows-packaging analysis, no DB CLIs.
Every chat pays that cold-start tax again (rootfs and `/home` are ephemeral
per pod), and the LLM is never *told* what exists — the upstream-verbatim
`/system` prompt says "a computer", not "pandas 2.x, R 4.2, pwsh 7, duckdb".

The workbench change makes the base image a capable, **curated** workbench
(~4 GB compressed) and adds a **capability manifest** so the model sees the
inventory in-band through `/system`, plus a narrow `sudo apt-get` whitelist
for the long tail the manifest cannot cover.

## What Changes

- **`rust/runtime/tools.json` — single source of truth.** A stdlib-JSON
  manifest of tool areas (Archives, General CLI, Python/data, R, Docs depth,
  Windows packaging, RE light, DB clients) with their apt and pip package
  lists. `rust/runtime/Dockerfile` stage 3 expands it via
  `rust/runtime/gen-manifest.py --print-apt/--print-pip` into
  `apt-get install --no-install-recommends` / system-wide pip — the manifest
  and the image contents can never drift (no second hand-maintained list).
  Tarball-distributed tools (Node 22 LTS, PowerShell 7, .NET SDK 8, CFR,
  duckdb CLI) stay pinned VERSION+SHA256 in the Dockerfile; unrar needs the
  `non-free` apt component (freeware license — NOTICE.md).
- **PEP-668 relief + sudo apt whitelist.** `EXTERNALLY-MANAGED` is removed so
  tenant `pip install --user` works; `sudo` is installed with
  `/etc/sudoers.d/sandbox` allowing **only** `apt-get`
  (update/install/remove/purge/upgrade/full-upgrade/clean/autoremove),
  validated with `visudo -c`. Nothing else gets sudo; npm/pip stay
  user-mode. Apt maintainer scripts run as root *inside gVisor* against an
  ephemeral rootfs — residual risk documented in `docs/security.md`.
- **Capability manifest → LLM awareness.** `gen-manifest.py` (stdlib json)
  probes key tool versions at build time and writes
  `/usr/local/share/sandbox-capabilities.md` (~80 lines, ≤500 tokens): pure
  per-area `name — version` inventory + a build-time dpkg base count +
  `sandbox-tools` hint — deliberately **path-free**. `/usr/local/bin/sandbox-tools`
  prints that file plus a live delta (re-probed versions, current dpkg count
  vs the baked base). Runtime knob `SANDBOX_TOOLS_MANIFEST` (env; empty/unset
  = disabled) makes `GET /system` append the file as a
  `## Available toolchain (base image)` section **after** template expansion —
  the upstream-verbatim default prompt stays byte-for-byte pinned when the
  knob is off (existing unit test untouched), and the append also applies to
  an operator-overridden prompt. Chart value `sandboxTemplate.toolsManifest`
  (default `true`) renders the env var.
- **Workspace conventions section (config-driven, no hardcoded paths).**
  `system.rs` builds a second appended section — `## Workspace conventions` —
  in Rust from the *configured* workspace root (the same `WORKDIR` config the
  runtime uses for path resolution; fallback = the config default, never a
  literal in string-building code): scratch files belong in
  `{workdir}/tmp` (mkdir -p if missing), `{workdir}` root is for
  deliverables, `/tmp` is tmpfs wiped on restart while `{workdir}` persists,
  plus the install recipes (venv at `{workdir}/.venv` persists;
  `pip install --target /packages/py` + PYTHONPATH session-local;
  `npm config set prefix /packages/npm`; `sudo apt-get` writes the ephemeral
  rootfs). Gated by the same `SANDBOX_TOOLS_MANIFEST` knob, appended after
  the toolchain section. The static manifest file cannot carry these — it is
  baked at build time while `WORKDIR` varies per deployment. `sandbox-tools`
  prints the same conventions dynamically (`${WORKDIR:-/workspace}`).
- **Docs.** New `docs/toolchain.md` (inventory table by area with
  versions-at-release, install recipes, persistence semantics, `/packages`
  off-PATH rationale, psql/mysql absence rationale), `docs/security.md` sudo
  posture + residual risk, `docs/compatibility.md` divergence row (the
  `/system` append), `mkdocs.yml` nav, `NOTICE.md` license table for the new
  components, `CHANGELOG.md` `[Unreleased] ## Added`.
- **Tests.** Rust unit tests: config default for the knob; prompt pinned
  upstream-verbatim with the knob unset/manifest absent; appended toolchain +
  conventions sections with a tmpfile manifest; append applies to the
  operator-overridden prompt; with a non-default `WORKDIR` (`/data/ws`) the
  prompt contains `/data/ws/tmp` and **not** `/workspace/tmp`. e2e
  `tests/e2e/test_toolchain.py`: `/system` contains `Available toolchain` +
  `pandas` + the `/workspace/tmp` conventions line; `sandbox-tools` exits 0
  and shows the conventions; `sudo apt-get install figlet` succeeds;
  `sudo rm /tmp/x` denied; `pip install --target /packages/py` + PYTHONPATH
  import works; plus a no-cluster self-test that `gen-manifest.py` renders
  `tools.json`.

## Impact

- Affected: `rust/runtime` (`Dockerfile`, new `tools.json` +
  `gen-manifest.py`, `config.rs` knob, `system.rs` append + tests),
  `open-websandbox-platform/chart` (`values.yaml`, `values.schema.json`,
  `templates/sandboxtemplate.yaml`), `tests/e2e` (new module), `docs/`,
  `NOTICE.md`, `CHANGELOG.md`, `openspec/`.
- Not affected: broker, router, upstream agent-sandbox vendoring, network
  policy (all new tools use the already-permitted public-internet egress;
  DB clients beyond sqlite3/duckdb are deliberately absent — RFC1918 is
  blocked, tenants can `sudo apt-get install` psql/mysql if they truly need
  them against non-private endpoints).
- Image size: runtime grows from ~1 GB to ~4 GB (Node + .NET SDK 8 +
  PowerShell + JDK + R + Python science stack + tesseract/ocrmypdf +
  LibreOffice retained). Accepted cost of one fat image (see design.md);
  pull is amortized per node and hidden by the warm-pool/cold-start path.
- Wire-compat: `/system` response text gains optional appended sections when
  the knob is on (default on); with the knob off the prompt is
  byte-for-byte upstream v0.12.3 — the pinned unit test is the proof.
