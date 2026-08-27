# Tasks

- [x] e2e.yml: `drain` matrix arm (values-kind-pvc.yaml + E2E_DRAIN=1, drain module only)
- [x] local KIND (exposed the #147 relay-auth bug) verification: E2E_DRAIN=1 runs (not skips) against values-kind-pvc.yaml
- [x] docs: operations.md drain lane runs in CI; CHANGELOG
- [x] #147: relay authenticates upstream WS (per-session key, fail-closed) + ws_relay header assertion
