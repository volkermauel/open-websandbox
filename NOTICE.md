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
| [`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox) controller + CRDs (pinned **v0.5.3**) | Apache License 2.0 | `open-websandbox-platform/upstream/` (vendored, SHA256-pinned) |
| [gVisor (`runsc`)](https://gvisor.dev/) RuntimeClass | Apache License 2.0 | installed on cluster nodes; not distributed in this repository |

Third-party Rust / Go / Python dependencies retain their respective licenses
(declared in each manifest: `rust/Cargo.toml`, the router module, and
`requirements-test.txt`). The full dependency advisory/license picture is
gated in CI via `cargo deny` and `cargo audit`.
