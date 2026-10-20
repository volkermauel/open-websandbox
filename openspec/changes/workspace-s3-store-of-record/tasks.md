# Tasks: S3 store-of-record

## 1. Config + chart

- [ ] 1.1 `shared/config.rs`: `persistentMode: s3-record` (+ legacy
      `per-user-pvc` transition value); scratch VCT knobs
      (`scratch.storageClass`, `scratch.size`)
- [ ] 1.2 chart pool template: render `volumeClaimTemplates` from values;
      base template unchanged; `volumeClaimTemplatesPolicy: Allowed`
- [ ] 1.3 quota docs/defaults: PVC count = warm replicas + live chats

## 2. Broker

- [ ] 2.1 resolve: s3-record branch — claim → warm bind → restore-before-
      ready (existing seam); drop subPath injection
- [ ] 2.2 reaper: suspend = snapshot→S3 → delete claim (GC chain); drop
      park-to-PVC path for s3-record
- [ ] 2.3 draft adoption → S3 server-side copy; drop `move_workspace_dir`
- [ ] 2.4 drop per-user PVC APIs (`ensure_workspace_pvc`, prefix layout)
      behind the legacy transition flag

## 3. Migration

- [ ] 3.1 lazy per-chat migration on first resolve (PVC subPath → S3 key)
- [ ] 3.2 bulk sweep Job for idle chats + verification (size/hash) before
      PVC deletion
- [ ] 3.3 RustFS bucket lifecycle rule: docs + apply script

## 4. e2e + docs

- [ ] 4.1 `e2e-s3-record` lane: write → suspend(archive) → resolve(restore)
      → content identical; reap → PVC GC observed
- [ ] 4.2 docs rewrite: deploy (storage chapter), operations (flows,
      retention), architecture (model diagram)
- [ ] 4.3 full cargo suite + KIND lanes green
