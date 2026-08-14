<!-- Thanks for contributing! Link the issue(s) this closes, then walk the checklist. -->

## Summary

<!-- What does this change do, and why? One short paragraph. -->

Closes #

## What changed

<!-- Bulleted list of the notable changes. Note any breaking changes / Helm value renames. -->

-

## Verification

This repo enforces "test locally before you push" (R2 in `AGENTS.md`). Tick
the checks you actually ran green locally:

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `helm lint open-websandbox-platform/chart/`
- [ ] `helm template open-websandbox open-websandbox-platform/chart/ -f open-websandbox-platform/chart/values-kind.yaml`
- [ ] Docs change → `mkdocs build --strict` is clean
- [ ] Chart values change → `values.schema.json` still validates and `additionalProperties` is respected

<!-- If this touches the control plane, paste a one-line summary of the local
     KIND e2e result (e.g. "runc 10/10, gVisor 10/10, s3-tiered 3/3"). -->

## Notes for review

<!-- Anything reviewers should look at closely: security implications, an
     intentional trade-off, a follow-up issue this enables. -->
