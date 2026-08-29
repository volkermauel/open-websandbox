# NOTICE

**open-websandbox**
Copyright © the open-websandbox contributors.

This program is free software: you can redistribute it and/or modify it under
the terms of the **GNU Affero General Public License v3.0 only**
(`AGPL-3.0-only`) as published by the Free Software Foundation. The full
license text is in [`LICENSE`](./LICENSE).

## Vendored / relied-upon components

| Component | License | Where |
|-----------|---------|-------|
| [`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox) controller + CRDs (pinned **v0.5.6**) | Apache License 2.0 | `open-websandbox-platform/upstream/` (vendored, SHA256-pinned) |
| [gVisor (`runsc`)](https://gvisor.dev/) RuntimeClass | Apache License 2.0 | installed on cluster nodes; not distributed in this repository |
| [LibreOffice](https://www.libreoffice.org/) (`-nogui` Debian packages) | Mozilla Public License 2.0 | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [Node.js](https://nodejs.org/) v22 LTS tarball (incl. npm; corepack-provided pnpm/yarn) | MIT (Node.js license; bundled deps retain their own) | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [PowerShell](https://github.com/PowerShell/PowerShell) 7.6.5 tarball | MIT | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [.NET SDK](https://dotnet.microsoft.com/) 8.0.424 tarball (incl. the `ilspycmd` dotnet tool) | MIT | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [PSAppDeployToolkit](https://psappdeploytoolkit.com/) 4.1.8 (ModuleOnly zip) | GNU LGPL v3.0 (module's `COPYING.Lesser`) | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [CFR](https://github.com/leibnitz27/cfr) java decompiler 0.152 | MIT | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [duckdb](https://duckdb.org/) CLI v1.5.5 | MIT | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [unrar](https://www.rarlab.com/) (Debian `unrar` package) | freeware — Debian `non-free` (license forbids using its source to re-create the RAR compression algorithm); distribution of unmodified binaries is permitted | installed in the runtime container image by `rust/runtime/Dockerfile` (the only reason the build enables the `non-free` apt component); not distributed in this repository |
| [pandoc](https://pandoc.org/) (Debian package) | GPL-2.0-or-later | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [msitools](https://gitlab.gnome.org/GNOME/msitools) (Debian package, provides `wixl`) | LGPL-2.1-or-later | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [radare2](https://rada.re/) (Debian package) | LGPL-3.0-or-later | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [tesseract](https://github.com/tesseract-ocr/tesseract) (Debian package) | Apache License 2.0 | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |
| [ocrmypdf](https://ocrmypdf.readthedocs.io/) (Debian package) | Mozilla Public License 2.0 | installed in the runtime container image by `rust/runtime/Dockerfile`; not distributed in this repository |

Third-party Rust / Go / Python dependencies retain their respective licenses
(declared in each manifest: `rust/Cargo.toml`, the router module, and
`requirements-test.txt`). The full dependency advisory/license picture is
gated in CI via `cargo deny` and `cargo audit`.
