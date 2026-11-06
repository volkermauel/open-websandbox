# Tasks: desktop/browser sidecar in the sandbox pod (chart)

## 1. Values + schema

- [x] 1.1 Add `desktop` block to `values.yaml`: `enabled` (true), `image`,
      `apiKey` (""), `url` (http://127.0.0.1:8000), `shmSize` (512Mi),
      `resources` (200m/512Mi requests, 1/1Gi limits).
- [x] 1.2 Declare `desktop` in `values.schema.json` (strict
      `additionalProperties: false`) incl. `containerResourcesHalf` definition.

## 2. Template

- [x] 2.1 `templates/sandboxtemplate.yaml`: resolve `$desktopKey` once per
      render (values or `randAlphaNum 48`).
- [x] 2.2 Runtime container env `DESKTOP_URL`/`DESKTOP_API_KEY`, gated on
      `desktop.enabled`.
- [x] 2.3 `desktop` sidecar container: restricted securityContext per the
      spike (drop ALL caps, no priv-esc, uid/gid = `sandboxTemplate.uid`,
      RuntimeDefault seccomp), spike env contract
      (`HOME`, `OPEN_TERMINAL_API_KEY`, `OPEN_TERMINAL_ENABLE_DESKTOP/BROWSER`,
      `OPEN_TERMINAL_BROWSER_NO_SANDBOX`), resources, `/dev/shm` mount.
- [x] 2.4 `desktop-shm` Memory emptyDir with `desktop.shmSize` sizeLimit.

## 3. Profiles + docs

- [x] 3.1 `desktop.enabled: false` in all nine `values-kind*.yaml` e2e
      profiles.
- [x] 3.2 `docs/deploy.md`: must-override row (`desktop.image` pin by digest)
      + §3 desktop sidecar section.

## 4. Verification

- [x] 4.1 `helm lint` clean.
- [x] 4.2 `helm template` renders: sidecar + env + volume on (default);
      zero desktop traces off; custom `shmSize`/`apiKey`/resources honored;
      schema rejects invalid values (e.g. `shmSize: 999`).
