# Tasks: agent-sandbox v1.0.7 upgrade

## 1. Vendored swap

- [x] 1.1 `upstream/`: `sandbox-with-extensions-v1.0.7.yaml` (sha256
      `136c10c9…`), `VERSION`, `SHA256SUMS`
- [x] 1.2 `chart/crds/upstream-agent-sandbox-v1.0.7-crds.yaml` (4 CRDs,
      v1beta1-only, conversion stanza removed)
- [x] 1.3 `chart/files/upstream-agent-sandbox-v1.0.7.yaml` (webhook
      Service/Role/RoleBinding absent; controller ClusterRole without CRD
      verbs)
- [x] 1.4 `templates/upstream/upstream.yaml` + values comments → v1.0.7

## 2. Docs

- [x] 2.1 Pin references: README diagram, `docs/{index,deploy,operations,
      quickstart,architecture,container-snapshots}.md`
- [x] 2.2 Ops runbook (private ops repo): CRD-first apply order +
      `storedVersions` check + orphaned-webhook cleanup

## 3. Verification

- [x] 3.1 `helm lint` + `helm template`: controller image
      `agent-sandbox-controller:v1.0.7`, zero webhook objects
- [x] 3.2 Cluster (ops): CRDs applied → `storedVersions == [v1beta1]` on all
      four → controller rolls clean → orphaned webhook objects removed
- [x] 3.3 Smoke: persistent resume (3.5s) + ephemeral warm claim (0.46s)
- [ ] 3.4 KIND e2e suite green (controller image from chart)
- [x] 3.5 No release tag needed (our images unchanged — only the vendored
      controller + CRDs moved; chart publishes via the chart workflow)
