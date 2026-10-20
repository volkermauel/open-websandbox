# Tasks: ephemeral VCT scratch pool

## 1. Chart

- [ ] 1.1 `values.yaml`: `warmPool.scratch: {enabled: true, storageClass: "",
      size: 5Gi}` + schema entries (empty `storageClass` = cluster default)
- [ ] 1.2 `templates/sandboxtemplate.yaml` pool branch: render
      `spec.volumeClaimTemplates` (workspace VCT) from values; keep the
      emptyDir `workspace` volume (VCT volume overrides same-named template
      volume upstream)
- [ ] 1.3 Verify render: pool template carries the VCT; base template does
      not; `helm lint` clean

## 2. Verification

- [ ] 2.1 KIND (pool enabled + local-path): pool member boots with bound
      PVC; claim adopts; execute writes to `/workspace`; reap deletes claim
      → Sandbox → PVC (ownerRef GC observed)
- [ ] 2.2 Cluster runbook: quota headroom check, pool recycle on template
      change, warm-bind latency unchanged
- [ ] 2.3 Full cargo suite green (no broker changes expected)

## 3. Docs

- [ ] 3.1 `docs/operations.md` warm-pool section: VCT scratch model, GC
      chain, quota math
- [ ] 3.2 `docs/deploy.md` values reference + `values-kind` note (pool
      disabled there)
