# Tasks: workbench toolchain image + capability manifest

## 1. Single source of truth + image

- [x] 1.1 `rust/runtime/tools.json`: stdlib-JSON areas (Archives, General
      CLI, Python/data incl. pip list, R, Docs depth, Windows packaging, RE
      light, DB clients) with the approved apt/pip package contents
- [x] 1.2 `rust/runtime/gen-manifest.py` (stdlib json): `--print-apt` /
      `--print-pip` (Dockerfile consumption), manifest rendering
      (per-area name—version probes, dpkg base count, path-free footer,
      `--self-test` mode)
- [x] 1.3 `rust/runtime/Dockerfile` stage 3: non-free component (unrar),
      python3+ca-certificates bootstrap RUN, COPY tools.json + generator,
      apt/pip expansion RUNs (`--no-install-recommends`, apt lists cleanup),
      Node 22 LTS tarball (pinned VERSION+SHA256, /opt/node, corepack),
      PowerShell 7 + .NET SDK 8 tarballs (pinned, /opt, PATH/DOTNET_ROOT,
      DOTNET_CLI_HOME=/opt, ilspycmd, wix-if-installable), PSAppDeployToolkit
      module, CFR jar + wrapper, duckdb CLI, `fd` symlink, sudo package +
      sudoers.d/sandbox (visudo -c), EXTERNALLY-MANAGED removal, keep
      LibreOffice/user/env/ENTRYPOINT intact
      (wix resolved as intentionally SKIPPED — Windows-only; `wixl` is the
      documented MSI path)

## 2. Capability manifest → LLM awareness

- [x] 2.1 `config.rs`: `tools_manifest` knob (env `SANDBOX_TOOLS_MANIFEST`,
      empty/unset = disabled); defaults + parse tests
- [x] 2.2 `system.rs`: append `## Available toolchain (base image)` (manifest
      file content) + `## Workspace conventions` (built from the configured
      `WORKDIR`, no hardcoded `/workspace`) after template expansion; knob
      off / missing file ⇒ prompt byte-for-byte upstream (pinned test
      unchanged); unit tests incl. non-default-WORKDIR pin
- [x] 2.3 `/usr/local/bin/sandbox-tools` (in image): manifest + live delta
      (re-probes, dpkg count vs baked base) + dynamic conventions
- [x] 2.4 Image `ENV SANDBOX_TOOLS_MANIFEST=…`; chart
      `sandboxTemplate.toolsManifest` (default true) in values.yaml +
      values.schema.json + sandboxtemplate.yaml env (explicit `""` when
      false)

## 3. Tests

- [x] 3.1 Rust unit tests (config default, upstream pin with knob off,
      tmpfile append, override-prompt append, WORKDIR=/data/ws pin,
      conventions gated on knob)
- [x] 3.2 `tests/e2e/test_toolchain.py`: /system sections via relay,
      sandbox-tools, sudo apt install figlet, sudo rm denied,
      pip --target + PYTHONPATH; gen-manifest.py self-test (no broker)

## 4. Docs & release

- [x] 4.1 `docs/toolchain.md` (inventory table, recipes, persistence
      semantics, off-PATH rationale, psql/mysql rationale) + mkdocs.yml nav
- [x] 4.2 `docs/security.md`: sudo posture + residual risk; residual-risk
      list entry
- [x] 4.3 `docs/compatibility.md`: new divergence row + provenance note
      ("configured workspace root (WORKDIR, default /workspace)")
- [x] 4.4 `NOTICE.md` license table for new components; `CHANGELOG.md`
      `[Unreleased] ## Added`

## 5. Gates

- [x] 5.1 `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` +
      `cargo test --workspace`
- [x] 5.2 `helm lint` + `helm template -f values-kind.yaml`; `openspec
      validate`; `mkdocs build --strict`; `pytest tests/e2e --collect-only -q`
- [ ] 5.3 Local `docker build` (e2e.yml context/flags mirrored) + bounded
      smoke test; image size recorded for the PR body
