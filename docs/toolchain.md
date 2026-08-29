# Workbench toolchain

The default runtime image is a curated workbench: a sandbox chat can unpack archives,
edit documents, run Python/R data work, build Windows installers, and decompile binaries
without a first-chat bootstrap. The image contents and what the LLM is told about them
come from **one source of truth**, `rust/runtime/tools.json`:

- `rust/runtime/gen-manifest.py --print-apt` / `--print-pip` expand it into the
  `Dockerfile` installs, and the same file renders the capability manifest baked into
  the image. The manifest and the image cannot drift.
- `SANDBOX_TOOLS_MANIFEST` (set in the image + chart, default on) makes `GET /system`
  append `## Available toolchain (base image)` + `## Workspace conventions` after the
  upstream-verbatim prompt. Unset or empty ⇒ the prompt is byte-for-byte upstream again
  (see [open-terminal compatibility](compatibility.md)).
- `sandbox-tools` (in the image) re-prints the manifest **plus** a live delta: key tool
  versions re-probed now and the current dpkg count vs the build-time base count.

## Inventory by area

Versions for apt-managed tools are Debian bookworm's at image build; tarball-distributed
tools are pinned VERSION + SHA256 in `rust/runtime/Dockerfile`.

| Area | Highlights |
|------|------------|
| Archives | gzip, bzip2, xz, zip/unzip, 7z (p7zip-full), unar, unrar, lz4, cpio, cabextract |
| General CLI | git, curl, wget, jq, ripgrep, fd, less, nano, tree, htop, shellcheck, openssh-client, dnsutils, iproute2 |
| Python & data | python3 + venv/pip/dev, build-essential; pip: numpy, pandas, scipy, matplotlib, openpyxl, python-docx, python-pptx, pyarrow, duckdb, oletools, capstone, pip-audit |
| R | r-base-core + dplyr, tidyr, ggplot2, readxl, stringr, lubridate |
| Docs depth | pandoc, poppler-utils, qpdf, ghostscript, imagemagick, exiftool, tesseract-ocr (+deu), ocrmypdf, antiword, LibreOffice headless (`soffice`) |
| Windows packaging | msitools (`msiextract`/`msiinfo`/`msibuild`), makensis (Debian `nsis`), innoextract, PowerShell 7.6.5, .NET SDK 8.0.424 (+ ilspycmd 9.1.0.7988), PSAppDeployToolkit 4.1.8 |
| Reverse engineering (light) | binutils, gdb, binwalk, yara, JDK (default-jdk-headless), CFR 0.152 (`cfr`) |
| DB clients | sqlite3, duckdb CLI v1.5.5 |
| Node | Node.js 22 LTS (v22.23.2 tarball, /opt/node) with npm and corepack-managed pnpm + yarn |

Adding a tool means editing `tools.json` only — the Dockerfile and the `/system` manifest
follow. Run the self-test after editing:

```bash
python3 rust/runtime/gen-manifest.py --self-test
```

## Install recipes (workspace-relative)

Recipes refer to **the configured workspace root** (`WORKDIR`, default `/workspace`) —
the same root the runtime's conventions section is rendered from.

- **Scratch files** belong in `${WORKDIR}/tmp` (`mkdir -p "${WORKDIR}/tmp"`); keep the
  workspace root for deliverables.
- **System packages** (available to anything in the sandbox):

  ```bash
  sudo apt-get update && sudo apt-get install -y figlet
  ```

  This writes the **ephemeral rootfs** — reinstall after a pod restart. Nothing except
  the apt-get verbs listed below gets sudo.
- **Session-local Python deps** (off `PATH`, see below):

  ```bash
  pip install --target /packages/py <pkg>
  PYTHONPATH=/packages/py python3 …
  ```

  The PEP-668 `EXTERNALLY-MANAGED` marker is **removed** in the image, so plain
  `pip install` works — no `--break-system-packages`.
- **Persistent Python env** (survives pod restarts, lives on the workspace PVC):

  ```bash
  python3 -m venv "${WORKDIR}/.venv" && . "${WORKDIR}/.venv/bin/activate"
  ```
- **npm user prefix** (keeps npm's global dir out of the ephemeral rootfs):

  ```bash
  npm config set prefix /packages/npm
  ```

## Persistence semantics

| Location | Persists? | Notes |
|----------|-----------|-------|
| `${WORKDIR}` (default `/workspace`) | **Yes** (per-user PVC, when `broker.persistentMode` is on) | work files, `${WORKDIR}/.venv` |
| rootfs (`/usr`, `/etc`, …) | No — rebuilt per pod | `sudo apt-get` installs vanish on restart |
| `/home/sandbox` | No — ephemeral per pod | no `.bashrc` autoload ⇒ no cross-session code execution |
| `/packages` | No — ephemeral per pod | deliberately **not on `PATH`** |
| `/tmp` | No — tmpfs, wiped | hard-capped (2 GiB) |

`/packages` staying off `PATH` is an anti-shadowing choice: nothing a session `pip`/
`npm`-installs there can silently replace a binary or module another command expects.
Imports/prefixes opt in explicitly (`PYTHONPATH=…`, `npm prefix`), per command.

## Deliberately absent, and how to get them

- **psql / mysql clients** — not baked in (the image is a workbench, not a DB bastion;
  every image resident is attack + license surface for all tenants). Get them on demand:

  ```bash
  sudo apt-get update && sudo apt-get install -y postgresql-client  # psql
  sudo apt-get update && sudo apt-get install -y default-mysql-client  # mariadb client
  ```

  Ephemeral rootfs semantics apply (reinstall after a pod restart). Remember the
  default-deny NetworkPolicy: only public-internet DNS/HTTP/HTTPS egress is allowed —
  RFC1918 database hosts are unreachable by design.
- **WiX (`wix` dotnet tool)** — intentionally **not** installed: it warns at runtime that
  only Windows is supported ("All behavior after this point is undefined").
  **`wixl` is not packaged by Debian either** — bookworm, bookworm-backports and
  trixie all ship `msitools` *without* wixl (verified empirically). So this image has
  no WiX-syntax MSI builder; the supported silent-deployment paths are:
  [PSAppDeployToolkit](https://psappdeploytoolkit.com/) wrapper packages (the
  NinjaOne-standard wrapper — wraps ANY installer incl. existing MSIs with silent
  `msiexec` flags), `makensis` for new installers, and msitools'
  `msiextract`/`msidump`/`msiinfo` for inspecting + extracting existing MSIs.
  Raw MSI assembly is possible via `msibuild` (its own project format, not WiX
  syntax). A `wine` + WiX lane remains a possible opt-in heavyweight profile.
- **radare2 / upx-ucl** — not in Debian bookworm at all (any component: main,
  contrib, non-free; both were dropped from Debian 12). The RE-light area keeps
  binutils, gdb, binwalk, yara, the JDK, and CFR. Building radare2 from upstream
  sources is possible in a session (`sudo apt-get install -y build-essential` is
  already satisfied) but is not a supported image resident.

## sudo: apt-get only

`/etc/sudoers.d/sandbox` grants passwordless sudo for **exactly** the apt-get verbs
`update, install, remove, purge, upgrade, full-upgrade, clean, autoremove` — nothing else.
Every invocation (allowed or denied) is logged to `/var/log/sudo.log` inside the sandbox
(world-readable).

> **gVisor nodes**: setuid elevation requires runsc's `allow_suid` setting
> ([gVisor #5299](https://github.com/google/gvisor/issues/5299) — runsc mounts container
> filesystems `nosuid` by default). [`infra/gvisor/install-gvisor-node.sh`](../infra/gvisor/install-gvisor-node.sh)
> and [`infra/kind/install-runsc.sh`](../infra/kind/install-runsc.sh) write
> `/etc/runsc/config.toml` with `allow_suid = true` and wire it into containerd via
> `options.ConfigPath` — re-run them if `sudo` fails with "effective uid is not 0".
`npm`, `pip`, and friends stay user-mode by design. The security posture — why running
apt maintainer scripts as root inside the sandbox is acceptable, and the
`readOnlyRootFilesystem` implication — is on the [security model](security.md) page.
