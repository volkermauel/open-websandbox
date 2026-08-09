# Per-session broker↔runtime API key (native k8s, stateless broker)

Issue: #4. Replaces the single shared `RUNTIME_API_KEY` (one credential authenticating
the broker to **every** runtime pod) with a **per-session** key, hard cutover, no
backward compatibility.

## Why

Today the broker authenticates to every sandbox runtime pod with **one** shared
`RUNTIME_API_KEY` (chart-resolved, defaulting to `BROKER_SHARED_SECRET`; injected into
both the broker env and the runtime env via `owui-runtime-secret`). A single shared
inter-component credential means compromising (or even reading the env of) **one** runtime
pod yields the key the broker uses against **all** pods — a lateral-movement / blast-radius
problem across every user's sandbox. Issue #4 asks for a **per-session** key with a
stateless broker, HA-compatible, rotate-on-resume, and a hard cutover.

## Owner decisions (issue #4 — implemented exactly)

1. **Key delivery** → a per-session Secret, scoped/mounted only into that sandbox pod.
2. **Key store** → a kubernetes-native per-session **Secret**; the **broker stays stateless**
   (reads the Secret per session; no in-memory/leader state, no database).
3. **Rotation** → rotate-on-resume (mint a fresh key when a parked/ephemeral session resumes).
4. **Backward-compat** → **none: hard cutover** — remove the shared `RUNTIME_API_KEY`.

## The critical constraint (investigated first) and the chosen delivery

The sandbox pods are created by the **vendored, byte-for-byte-preserved** upstream
`kubernetes-sigs/agent-sandbox` controller (CRDs `agents.x-k8s.io` /
`extensions.agents.x-k8s.io`, v0.5.3) from a **shared `SandboxTemplate`**. Three findings
shape the design:

1. **The runtime pod is network-isolated from the API server.** `networkpolicy-runtime.yaml`
   is default-deny with egress **only** to public DNS + HTTPS/HTTP to the internet, **excluding
   all RFC1918/link-local**, and the template sets `dnsPolicy: None` +
   `automountServiceAccountToken: false`. So the runtime **cannot read a Secret via the k8s
   API** — doing so would require punching a hole in this anti-lateral-movement NetworkPolicy
   (a security regression) and re-enabling cluster DNS. The SA-token-read fallback is therefore
   **rejected**.

2. **The v1beta1 `SandboxClaim` cannot carry a per-session Secret.** Its `spec` only exposes
   `additionalPodMetadata`, `env` (**static `value` only — no `valueFrom`/`secretKeyRef`**),
   `volumeClaimTemplates`, and a **required** `warmPoolRef` (warm-pod **reuse**). It has no
   `podTemplate`/`volumes`. So a claim can neither project a Secret volume nor reference one
   via env. And because `warmPoolRef` reuses an already-running warm pod (created from the
   shared template before any session exists), no per-session value can be injected into it.

3. **The `Sandbox` CR (agents.x-k8s.io) carries a full per-instance `podTemplate`**
   (`volumes`, `containers/env`, …) and the controller honors it — the existing persistent
   path (`_create_chat_sandbox`) already clones the base template and overrides the
   `workspace` volume. So a **broker-created direct `Sandbox`** is the one place a per-session
   projected Secret volume can be injected.

**Chosen delivery — projected per-session Secret volume via a broker-created direct `Sandbox`:**

- The broker mints a fresh high-entropy key per sandbox and writes it to a per-session Secret
  `owui-runtime-key-<sandbox-name>` (`stringData.api-key`), labeled
  `app.kubernetes.io/managed-by=owui-broker`.
- The broker creates the per-session `Sandbox` (both profiles) with a podTemplate that adds a
  **projected `secret` volume** `runtime-key` → `secretName: owui-runtime-key-<sandbox-name>`,
  mounted readOnly at `/etc/runtime-key`, item `api-key`. The Secret is created **before** the
  Sandbox so the (non-optional) volume is satisfiable at pod-creation time.
- The runtime reads its key **from the mounted file** `/etc/runtime-key/api-key` at boot (and
  reloads on auth-mismatch). **No k8s API access, no ServiceAccount token, no RBAC, no
  NetworkPolicy change** — `automountServiceAccountToken: false` and the isolated NetworkPolicy
  are preserved unchanged. The Secret is scoped/mounted into exactly one pod (true per-pod
  isolation), matching owner decision #1.

**Why not keep the warm pool for ephemeral?** Warm-pod reuse is fundamentally incompatible
with per-pod secret projection: warm pods are created from the shared template before a session
exists, their env/volumes are frozen at creation, and (per finding 2) a claim cannot name a
per-session Secret. Ephemeral therefore moves off `SandboxClaim`/warm-pool reuse onto a
**direct per-session `Sandbox`** (emptyDir workspace), unifying both profiles on the same
broker-created-Sandbox path the persistent profile already uses. The warm pool
(`warmpool.yaml`) is set to `replicas: 0` by default under per-session-key enforcement and
documented as incompatible until the upstream controller gains per-CR secret projection. This
trades ephemeral cold-start latency (already the norm for the default `persistent` profile)
for per-session isolation — a security, not a correctness, tradeoff.

## Proposal

- **`runtime/server.py`** — replace the shared `RUNTIME_API_KEY` (env) with a per-session key
  read from the mounted file `/etc/runtime-key/api-key`. `_validate_runtime_config()` refuses
  to boot if the key file is missing/empty (fail-closed, template-misconfig guard);
  `_auth_runtime()` validates the incoming Bearer against the file-backed key (constant-time),
  503 when unconfigured, 401 on mismatch, reload-on-mismatch for rotate-on-resume. Remove
  `RUNTIME_API_KEY`/`_PLACEHOLDER_API_KEYS`/`_runtime_api_key`.
- **`broker/main.py`** — mint a per-session key + persist to `owui-runtime-key-<sandbox>`;
  `_runtime_auth_headers(sandbox_name)` resolves the key via a **stateless** Secret get per
  hop; rotate-on-resume (re-mint + patch before flipping `operatingMode`→Running); create the
  per-session `Sandbox` (both profiles) with the projected `runtime-key` volume; reap the key
  Secret on sandbox deletion; **remove the shared `RUNTIME_API_KEY` path and the `SandboxClaim`
  warm-pool path** (ephemeral → direct `Sandbox`).
- **chart** — remove the shared `runtime-api-key` Secret injection (`owui-runtime-secret` +
  the broker `RUNTIME_API_KEY` env + `sandboxTemplate.runtimeApiKey`); add `secrets`
  create/get/patch/delete to the broker Role (it now owns per-session Secrets); remove the
  `RUNTIME_API_KEY` env from `sandboxtemplate.yaml`; set `warmPool.replicas: 0` default. **No
  runtime ServiceAccount/RBAC and no NetworkPolicy change** (the runtime reads a file, not the
  API). Vendored controller + CRDs untouched.
- **Tests** — update `tests/unit/runtime/test_runtime_auth.py` for the file-backed per-session
  key (keep the route-table auth invariant); update broker tests
  (`test_k8s`/`test_terminal_proxy`/`test_reaper`/`test_resolve`/`test_migrate`) for the
  per-session key + direct-Sandbox model; add a rotate-on-resume test.

## Decisions

- **D1 Delivery** — projected per-session Secret **volume** (file at `/etc/runtime-key/api-key`),
  not SA-token API read. Preserves the runtime NetworkPolicy isolation + `automount=false`;
  true per-pod scoping. (The SA-read fallback the issue sketched is blocked by the runtime
  NetworkPolicy — see finding 1.)
- **D2 Both profiles on direct `Sandbox`** — the `SandboxClaim` (warm-pool reuse, static-only
  env, no volumes) cannot carry a per-session Secret, so ephemeral joins persistent on the
  broker-created direct-`Sandbox` path.
- **D3 Stateless broker** — the broker reads the per-session Secret on each hop (no in-memory
  key cache); the Secret is the single source of truth, HA-safe across replicas.
- **D4 Rotate-on-resume** — re-mint + patch the Secret *before* resuming a parked sandbox; the
  new pod mounts the fresh value. Ephemeral "resume" = a fresh per-session Sandbox = a fresh key.
- **D5 Hard cutover** — no fallback to a shared key; `RUNTIME_API_KEY` removed everywhere.
- **D6 Warm pool disabled by default** — incompatible with per-pod secret projection;
  `warmPool.replicas: 0`. Revisit if upstream adds per-CR secret projection.
