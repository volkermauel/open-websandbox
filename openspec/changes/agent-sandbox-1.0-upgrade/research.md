# agent-sandbox v0.5.6 → v1.0.7 upgrade research

Date: 2026-10-20. Scope: kubernetes-sigs/agent-sandbox v0.5.6 (pinned) → v1.0.7 (latest).
Method: GitHub release notes (API), raw file fetches at both tags (blob-sha verified where noted),
local diff of vendored manifests. Primary sources only. No blog posts.

All file citations are `github.com/kubernetes-sigs/agent-sandbox/blob/<tag>/<path>` unless a
release-notes URL is given. `swe.yaml` = release asset `sandbox-with-extensions.yaml`.

---

## 1. Version delta table (v0.5.6 → v1.0.7), impact on us

We pin v0.5.6. Intermediates: v1.0.0, v1.0.1, v1.0.2, v1.0.3, v1.0.4, v1.0.5, v1.0.6, v1.0.7
(release index: https://github.com/kubernetes-sigs/agent-sandbox/releases).

| Ver | Date | Change (source) | Impact on open-websandbox |
|---|---|---|---|
| v1.0.0 | 2026-08-28 | **v1alpha1 removed; conversion webhook removed** (release notes, https://github.com/kubernetes-sigs/agent-sandbox/releases/tag/v1.0.0, PR #1470). All 4 CRDs serve/storage only `v1beta1`. Upgrade guide requires all 4 CRDs `storedVersions == [v1beta1]` before upgrade. Webhook svc/secret/CLI flags deleted; 4-step upgrade procedure in release notes + `docs/api-migration-guide.md`. | We only ever speak `v1beta1` (`rust/shared/src/crd.rs:193-194`, chart templates use `extensions.agents.x-k8s.io/v1beta1`). Migration = swap vendored CRDs; must verify `storedVersions`. Webhook removal simplifies our vendored slice. |
| v1.0.0 | 2026-08-28 | **`agents.x-k8s.io/pod-name` annotation no longer written** (v1.0.0 release notes, PR #1417). Sandbox name == pod name. | Broker never reads that annotation (grep: no hits in `rust/`). No impact. |
| v1.0.0 | 2026-08-28 | **`status.sandbox.serviceFQDN` added to SandboxClaim** (v1.0.0 notes, PR #1325; `extensions/api/v1beta1/sandboxclaim_types.go` `ServiceFQDN` field). Schema diff: only non-upstream schema addition in the whole range. | New optional field; kube-rs ignores unknown fields. Potential future simplification (see §6). |
| v1.0.0 | 2026-08-28 | Claim Ready condition reports **claim's own observedGeneration** (PR #1317); warm-candidate poll 500ms→100ms, burst-claim latency work (PRs #1454, #1417). | Faster warm binds; no API change. |
| v1.0.1 | 2026-09-03 | Label-domain allowlist **documented** for `additionalPodMetadata.labels` (`sandbox.users.io` default, `agent-sandbox-config` ConfigMap) (v1.0.1 notes, PR #1530). Enforcement pre-exists at v0.5.6 (`extensions/controllers/sandboxclaim_controller.go` @ v0.5.6 lines 1535-1572). | Broker currently sends `additional_pod_metadata: None` (`rust/broker/src/resolve.rs:439`). If we ever add claim labels, keys need an allowlisted domain prefix or ConfigMap config. |
| v1.0.2 | 2026-09-11 | Router authz interface + namespace change (PRs #1497, #1537) — router only, we self-build router from v0.5.6 vendored upstream; see chart `templates/router.yaml`. | Only if we re-pin the router. Note router moved to `agent-sandbox-system` ns upstream. |
| v1.0.3 | 2026-09-17 | Sandbox controller: **terminal `InvalidConfiguration` classification** for permanent apiserver errors (e.g. >63-char derived Service name) instead of hot-loop (PR #1630); new `SandboxReasonReconcilerError` (transient) (`api/v1beta1/sandbox_types.go` @ v1.0.7, constants ~lines 88-104). Namespace-terminating requeue fix (PR #1617). | Better failure ergonomics; broker renders conditions into error text only, no branching (`rust/broker/src/resolve.rs:744-771`). Low risk. |
| v1.0.4 | 2026-09-24 | **Kubernetes lifecycle Events** from Sandbox reconciler (`SandboxPodCreated/Ready/Suspended/Expired/PodSucceeded/PodFailed`), `--disable-sandbox-events` flag (PR #1532). | Observability gain; opt-out flag exists. No API impact. |
| v1.0.5 | 2026-10-01 | High-scale controller flags exposed in upstream Helm `controller.*` (`--api-connections`, `--cache-label-selectors`, `--sandbox-write-behind-window`, `--sandbox-warm-pool-replenish-delay`) (PR #1794; `helm/values.yaml` @ v1.0.7). `agent_sandbox_warmpool_size` metric (PR #1774). Warm-pool unschedulable detection reads mirrored `PodScheduled` from Sandbox.status (no Pod GETs, PR #1439). Cache-lag geometric backoff (PR #1768). | Perf only. We vendor raw manifests, not the upstream chart — flags reachable via Deployment args if needed. |
| v1.0.5 | 2026-10-01 | `SandboxWarmPool` printer column `Template` added (PR #1790). Confirmed: pool CRD v0.5.6 vs v1.0.7 printer-column diff. | Cosmetic. |
| v1.0.6 | 2026-10-08 | **CEL admission rule**: Sandbox with `spec.service: true` rejects `metadata.name` > 63 chars (PR #1475). Rule applies to existing objects — pre-existing long names fail every update incl. status. `x-kubernetes-validations` diff confirms: `!has(self.spec.service) \|\| !self.spec.service \|\| size(self.metadata.name) <= 63` is the **only** CEL delta in the range. | Our names (`owui-c-<hex>`) are far below 63 (`rust/shared/src/crd.rs` fixture). Risk only if name policy changes. |
| v1.0.6 | 2026-10-08 | **`--sandbox-claim-warm-candidate-grace-period`** (default 2s): how long a claim waits for a warm candidate PodIP before cold fallback (PR #1758; code `extensions/controllers/sandboxclaim_controller.go` @ v1.0.7, `warmCandidateGracePeriod`/`warmCandidateRetryAfter` ~lines 2040-2064, fallback log line 2243). | gVisor pods get IP slowly → if warm claims intermittently fall back to cold, raise this flag. Relevant knob for us. |
| v1.0.6 | 2026-10-08 | Warm pool `status.replicas/readyReplicas` freeze fix under 429 throttling (PR #1852). | Reliability. |
| v1.0.7 | 2026-10-15 | **Warm pools hold members waiting on scheduling gates**; stuck-member grace period now starts at Pod `Scheduled`, not Sandbox creation (PR #1817). Event action `WarmPoolNotProgressing` changed to hold-cause. | Protects gVisor pools against false "stuck" deletes during image pulls — directly relevant to our warm pool. |
| v1.0.7 | 2026-10-15 | Claim `Finished` condition uses claim's own observedGeneration (PR #1898); missing-template warning event + 1-min retry (PR #1572); `--enable-rest-client-metrics` toggle (PR #1571). | Claim-only spec change (e.g. `lifecycle.shutdownTime`) no longer stalls `kubectl wait --for=condition=Finished`. |
| v1.0.7 | 2026-10-15 | sandboxd: optional bearer-token auth (`--auth-token-sha256`), signal exit codes 128+n. Router traffic metrics. (v1.0.7 notes.) | Optional hardening for our runtime container if it ever runs sandboxd. |

### CRD schema delta (structural, v1beta1↔v1beta1)

Diff method: `openAPIV3Schema` property-tree flatten of `k8s/crds/*.yaml` at both tags.

- `Sandbox` + `SandboxTemplate`: **44 added properties, 0 removed** — all are upstream Kubernetes
  PodSpec evolutions (`httpGet.protocol`, `grpc.mode`, `volumeMounts[].bindMountOptions`,
  `evictionResponders`, `defaultUser`/`user` on projected volumes, `emptyDir.mode`).
  Sources: `k8s/crds/agents.x-k8s.io_sandboxes.yaml` @ v1.0.7 vs v0.5.6.
- `SandboxClaim`: only `status.sandbox.serviceFQDN` added. Spec unchanged
  (`spec`: `warmPoolRef`, `lifecycle`, `additionalPodMetadata`, `env`, `volumeClaimTemplates` in both).
- `SandboxWarmPool`: **zero schema delta** (spec `replicas`/`sandboxTemplateRef`/`updateStrategy`; status unchanged).
- Group/version: `agents.x-k8s.io` + `extensions.agents.x-k8s.io`, `v1beta1` storage+served in BOTH
  tags. v1.0.7 drops the `v1alpha1` served version and the `conversion: Webhook` stanza (CRD diff).
- CRD descriptions are stripped in released CRDs (`crd:maxDescLen=0`-style gen config), so doc text
  lives in the Go types — cite those instead (see §3).

### Manifest / RBAC / image delta (all-in-one `sandbox-with-extensions.yaml`)

- Object inventory v0.5.6 → v1.0.7: **removed** `Service agent-sandbox-webhook-service`,
  namespaced `Role`/`RoleBinding agent-sandbox-controller` (webhook TLS reader). Everything else identical in kind/name.
  Sources: `releases/download/v0.5.6/sandbox-with-extensions.yaml` vs `.../v1.0.7/...`.
- ClusterRole delta: `agent-sandbox-controller` **loses** `[get,patch,update] apiextensions.k8s.io/customresourcedefinitions`
  (was for the v0.5.x storage migration). `agent-sandbox-controller-extensions` unchanged.
- Controller Deployment delta: image tag only + removal of containerPort `9443 webhook`.
  Image both tags: `registry.k8s.io/agent-sandbox/agent-sandbox-controller:<tag>`.
- Our vendored copy is byte-identical to upstream v0.5.6:
  `open-websandbox-platform/upstream/SHA256SUMS` (`1696dbb6…`) == sha256 of the v0.5.6 release asset (verified).

---

## 2. Controller behavior changes affecting us

1. **Suspended condition semantics fixed** (v0.5.6 → v1.0.7, `api/v1beta1/sandbox_types.go` diff):
   - v0.5.6: "the controller does not currently remove this condition when the Sandbox is resumed,
     so a stale Suspended condition may linger after operatingMode returns to Running."
   - v1.0.7: "The condition is never removed: once the Sandbox runs again it reports False with
     reason NotSuspended."
   We patch `operatingMode` Suspended/Running (`rust/broker/src/sandbox.rs`). v1.0.7 is strictly
   better; only code that *infers* live state from condition *presence* must change — ours doesn't.
2. **Warm-pool member hold logic** (v1.0.7, PR #1817): scheduling-gated members held like
   unschedulable; grace period starts at `Scheduled`. Fewer false-positive deletes of gVisor pool members.
3. **Warm-candidate grace period** (v1.0.6, PR #1758): claim waits up to 2s (flag) for candidate
   PodIP before cold fallback. On slow-CNI/gVisor clusters raise it, else claims silently cold-start.
4. **Terminal InvalidConfiguration** (v1.0.3/v1.0.6): permanent failures stop requeueing and surface
   as `Ready=False reason=InvalidConfiguration` — also added to SDK `TERMINAL_CLAIM_READY_REASONS`
   (v1.0.5 notes). Broker could key off it for fast-fail (currently we time out).
5. **Status mirroring**: claim mirrors sandbox `name`, `podIPs` (both tags), `serviceFQDN` (new).
   Mirroring semantics unchanged otherwise; `podIPs` "cleared whenever the pod is absent … or while
   the Sandbox is suspended" (`sandboxclaim_types.go` @ v1.0.7). Broker polls `status.sandbox.name`
   then does pod-direct readiness (`rust/broker/src/resolve.rs:455-476`) — unaffected.
6. **networkPolicyManagement** (`SandboxTemplate.spec.networkPolicyManagement`, `Managed` default /
   `Unmanaged`): field already exists at v0.5.6 (`extensions/api/v1beta1/sandboxtemplate_types.go`
   lines 27-49 @ v0.5.6) — no delta in range; v1.0.7 only adds RL-SDK usage (PR #1861).
   Our per-template NPs unaffected.

---

## 3. THREAD 2 — PVC templates × warm pool × lifecycle (verdicts, quoted)

### 3.1 `SandboxClaim.spec.volumeClaimTemplates` semantics

- **Type**: `[]v1beta1.PersistentVolumeClaimTemplate` = `{metadata: EmbeddedObjectMetadata, spec:
  corev1.PersistentVolumeClaimSpec}` (`api/v1beta1/sandbox_types.go` @ v1.0.7 lines 218-226).
  Full PVC spec → `storageClassName`, `accessModes`, `resources.requests.storage`, `volumeMode`,
  `selector`, `dataSource` all supported. Nothing named `className`; standard `storageClassName`.
- **Creation**: claim controller merges claim VCTs (policy-gated) into `sandbox.Spec.VolumeClaimTemplates`
  (`extensions/controllers/sandboxclaim_controller.go` @ v1.0.7 lines 1797-1817, `mergeVolumeClaimTemplates`
  lines 1943-2010). Policy comes from `SandboxTemplate.spec.volumeClaimTemplatesPolicy`
  (`Disallowed` default → claim VCTs **rejected** with `ErrVolumeClaimTemplatesDisallowed` unless the
  template sets `Allowed` (append-only) or `Overrides` (replace-by-name)).
  `Sandbox.spec.volumeClaimTemplates` is **immutable** (CEL at `sandbox_types.go:285`).
- **PVC naming**: `<templateName>-<sandboxName>` — `controllers/sandbox_controller.go` @ v1.0.7
  lines 1720, 1511: `pvcName := pvcTemplate.Name + "-" + sandbox.Name`; pod gets a
  `PersistentVolumeClaimVolumeSource` volume of the same template name, and
  `MergeVolumeClaimVolumes` (lines 200-217, applied at 1521) lets VCT-backed volumes **override
  same-named** template volumes. One PVC per sandbox instance → per-sandbox isolation.
- **Created when**: first reconcile of the owning Sandbox (`reconcilePVCs` called unconditionally
  at `sandbox_controller.go:432` — applies to cold, claim-adopted, and pool-created sandboxes alike).
- **WHO deletes them**: nobody explicitly. PVCs get `SetControllerReference(sandbox, pvc)` (line
  1780) → **kube garbage-collector deletes the PVC when the owning Sandbox is deleted**; the PV then
  follows the StorageClass `reclaimPolicy` (upstream never touches `reclaimPolicy`/expansion —
  greps for `reclaimPolicy|allowVolumeExpansion|resize` in `sandbox_controller.go`: zero hits).
  Deletion chains:
  - claim deleted (cascade) → Sandbox deleted → PVC GC'd.
  - claim `shutdownPolicy: Delete|DeleteForeground` at expiry → same chain
    (`sandboxclaim_controller.go` @ v1.0.7 lines 369-380).
  - claim `shutdownPolicy: Retain` (default) at expiry → `reconcileExpired` deletes the **Sandbox**
    (lines 679-707: "Deleting Sandbox because Claim expired (Policy=Retain)") → PVC GC'd, claim object kept.
  - **Sandbox-level** expiry (`spec.shutdownTime`): `handleSandboxExpiry` deletes Pod + Service only;
    PVC is NOT deleted. With default `Retain` the Sandbox object survives → **PVCs survive expiry**
    (`sandbox_controller.go` lines 1797-1870: deletes pod/service, then only deletes the Sandbox
    object when `ShutdownPolicy == Delete`).
  - `operatingMode: Suspended`: "terminates the backing Pod while retaining the Sandbox object and
    its volumes" (`sandbox_types.go` @ v1.0.7, `SandboxOperatingModeSuspended` doc lines 243-247).
    → **suspend/resume preserves PVC data**; recreated pod re-mounts the same-name PVC.
- **Resize**: nothing upstream; standard `allowVolumeExpansion` applies. Upstream ships a worked
  example of online dynamic PVC expansion (`examples/openclaw-fleet-gke`, v1.0.3 notes PR #1652).

### 3.2 Does specifying `volumeClaimTemplates` on a claim force cold start? — CONFIRMED YES

Quote (in code, both tags, **unchanged in 1.0.x**):

`extensions/controllers/sandboxclaim_controller.go` @ v1.0.7 lines 2221-2226 (identical text @ v0.5.6 lines 2109-2114):

```go
// Implicit Cold Start Detection (Bypassing the Queue):
// If len(claim.Spec.Env) > 0 or len(claim.Spec.VolumeClaimTemplates) > 0, the controller immediately bypasses the warm pool queue.
if len(claim.Spec.Env) > 0 || len(claim.Spec.VolumeClaimTemplates) > 0 {
    logger.Info("Bypassing warm pool adoption because custom configuration is provided (env or volume claim templates)", "claim", claim.Name)
    return nil, nil
}
```

API doc for the same field — `extensions/api/v1beta1/sandboxclaim_types.go` @ v1.0.7:

```go
// volumeClaimTemplates is a list of persistent volume claims to be created for the sandbox.
// Specifying this field forces a cold start because warm pool pods will not have these volumes.
```

(and for `env`: "Please note adding this field means the Sandbox will always be cold-started from
the template of the warmpool.")

### 3.3 Any path where warm pool pods carry PVCs? — YES, via the SandboxTemplate

- Pool-created Sandboxes deep-copy the **entire** `SandboxBlueprint`, which includes
  `volumeClaimTemplates`: `extensions/controllers/sandboxwarmpool_controller.go` @ v1.0.7,
  `buildSandboxCR` lines ~1143-1235: `Spec: sandboxv1beta1.SandboxSpec{SandboxBlueprint:
  *template.Spec.SandboxBlueprint.DeepCopy()}` (comment at 1157: "with pod template and volume
  claim templates"). Core controller then creates the PVCs for each pool sandbox.
- Staleness detection covers VCTs: `compareSandboxBlueprint` → `compareVolumeClaimTemplates`
  (`sandboxwarmpool_controller.go` lines 1326-1352) — template VCT changes make pool members stale
  (recreated per `updateStrategy: Recreate`; kept until churn under `OnReplenish`, default).
- **Adoption keeps the PVCs**: `isAdoptable` (claim controller @ v1.0.7 lines 2879-2899) checks
  only deletionTimestamp, pool + template-hash labels, and SandboxWarmPool controllerRef — **no VCT check**.
  A claim adopting a warm sandbox takes over a sandbox that already has `<vct>-<sandboxName>` PVCs.
- Constraint: template VCTs are identical for every pool member (blueprint hash must match), so
  warm sandboxes give you *per-sandbox* PVCs with *template-identical* specs — not per-tenant specs.
- Alternate injection path: pre-create unowned PVCs labeled `agents.x-k8s.io/adoptable: "true"`
  (or carrying the sandbox tracking label) and the sandbox controller adopts them
  (`sandbox_controller.go` @ v1.0.7 lines 1733-1755; label const `sandbox_types.go:160`).

### 3.4 Sandbox-level / template-level PVC injection surviving adoption?

Yes — three supported mechanisms:
1. `Sandbox.spec.volumeClaimTemplates` (immutable CEL) / `SandboxTemplate.spec.…` blueprint field —
   "When creating a sandbox, PVCs will be created from these templates" (`sandbox_types.go` @ v1.0.7
   lines 263-270). Claim adoption transfers the Sandbox ownerRef; PVC ownerRef points at the
   Sandbox, so the PVC (and its data) survives the pool→claim→(park/suspend) lifecycle.
2. Pod-template `volumes[]` with `persistentVolumeClaim.claimName` referencing an external PVC —
   merged/overridden by VCT volumes of the same name (`MergeVolumeClaimVolumes`).
3. `agents.x-k8s.io/adoptable: "true"`-labeled pre-provisioned PVCs (see 3.3).

### 3.5 Anything in 1.0.x to drop broker-side PVC/S3 management?

- **S3 / offload hooks: NONE.** No S3/object-store code anywhere in `api/`, `controllers/`,
  `extensions/`, `internal/` (tree grep @ v1.0.7). Persistence features are:
  GKE PodSnapshot client-side extension + golden-warmpool example
  (`clients/python/.../gke_extensions/snapshots/`, `examples/podsnapshot-golden-warmpool/`),
  and a `recall-memory` example using projected SA tokens (PR #1913). GKE-specific, client-side.
- **TTL/GC**: claim-level `ttlSecondsAfterFinished` (timer from Finished condition LastTransitionTime,
  `internal/lifecycle/expiry.go` @ v1.0.7 `ExpireAt`/`TimeLeft`), claim `shutdownTime` +
  `shutdownPolicy` (Delete/DeleteForeground/Retain, default Retain — `sandboxclaim_types.go`),
  Sandbox-level `shutdownTime`/`shutdownPolicy` (Retain default — `sandbox_types.go` lines 326-348).
  These give controller-enforced teardown of Pod/Service/Sandbox — but **not** of PVCs beyond the
  ownerRef chain, and **no** data offload.
- **PVC reclaim**: purely k8s GC + StorageClass reclaimPolicy. No upstream assist.

**Conclusion**: the broker's per-user PVC + subPath + S3 offload-on-reap/restore-on-resume design
stays necessary for *durable across deletion* persistence. But claim/template VCTs could absorb the
*ephemeral per-chat scratch* case (PVC dies with the claim) — see §6.

---

## 4. Mapping to our code

Our usage (verified in `rust/`):
- Broker speaks `agents.x-k8s.io/v1beta1` Sandbox + `extensions.agents.x-k8s.io/v1beta1` SandboxClaim
  (`rust/shared/src/crd.rs:190-260`); patches `operatingMode` Running/Suspended and sets
  `shutdownPolicy: Retain` for parks (`crd.rs:32-34`, `:106-113`); claim spec uses ONLY
  `warmPoolRef` + (optionally) `additionalPodMetadata` — deliberately never `env`/`volumeClaimTemplates`
  so warm claiming works (`crd.rs:187-188`); polls `status.sandbox.name` then checks Pod readiness
  directly (`rust/broker/src/resolve.rs:455-476`); reaper deletes claims (cascade) for ephemeral.
- Persistent profile: broker-managed per-user PVC + per-chat subPath injected into the pod template
  (`resolve.rs:84-216`).
- Claims never set `env` → we keep warm-pool fast path (cold-start bypass never triggers for us).
- Chart: CRDs in `open-websandbox-platform/chart/crds/upstream-agent-sandbox-v0.5.6-crds.yaml`,
  non-CRD slice in `chart/files/upstream-agent-sandbox-v0.5.6.yaml` (includes webhook Service+certs
  — obsolete at v1.0.7), byte-identical to upstream release assets.

## 5. Migration risk list (ranked)

1. **CRD swap + storedVersions** (medium): replace vendored CRDs with v1.0.7 `k8s/crds/*` (apply
   before controller per v1.0.0 notes). Verify `status.storedVersions == [v1beta1]` on all 4 CRDs
   first; if `v1alpha1` lingers, apiserver rejects the CRD update. We never wrote v1alpha1 — expect clean.
2. **Vendored non-CRD slice** (low): `chart/files/upstream-agent-sandbox-v0.5.6.yaml` contains
   webhook Service/certs/Role/RoleBinding — drop them (deleted upstream); swap in v1.0.7 Deployment
   (args/flags changed: webhook flags gone; optional new flags), Service, ClusterRoles.
   Post-upgrade cleanup of orphaned webhook objects (v1.0.0 notes step 4).
3. **RBAC** (low): only delta is controller losing CRD get/patch/update; our broker ClusterRole
   (`chart/templates/broker.yaml:51-57`, groups unchanged) is untouched by upstream changes.
4. **Suspended condition semantics** (low): stale-lingering fixed; anything matching on condition
   *presence* must be re-checked — our broker reads conditions only for error strings.
5. **CEL name-length rule** (low): only bites `spec.service: true` + name > 63; ours are ~18 chars.
   Note rule bricks *existing* violating objects' status updates — irrelevant to us.
6. **Warm-candidate grace period** (low/ops): after upgrade, claims may cold-fallback after 2s on
   slow IPAM; watch cold-start rate, set `--sandbox-claim-warm-candidate-grace-period` if needed.
7. **kube-rs compatibility** (low): no group/version change; new status field ignored by serde.
8. **Router** (n/a unless re-pinned): upstream Go router moved to `agent-sandbox-system` ns and
   changed authz/proxy interfaces (v1.0.2/v1.0.6 notes) — matters only if we bump the vendored router.

## 6. Simplification opportunities (ranked)

1. **Drop conversion-webhook baggage** (free, do with upgrade): v1.0.7 all-in-one has no webhook
   Service/certs — smaller vendored slice, one less failure mode (private-cluster webhook firewall).
2. **Ephemeral scratch via template VCTs** (medium): if ephemeral chats need scratch disk, add
   `volumeClaimTemplates` to the pool's SandboxTemplate instead of broker-managed PVCs — every
   sandbox (incl. pool members) gets a PVC `scratch-<sandboxName>`, auto-deleted with the sandbox on
   reap (ownerRef chain). Requires emptyDir→PVC tradeoff (cost) and matching the template hash for
   the pool (pool recreate on template change). Does NOT fit persistent profile (PVC must outlive
   sandbox deletion there — broker PVC + S3 stays).
3. **Park/resume via Suspended instead of deleting pods ourselves** — already how we operate; v1.0.7
   Suspended-condition fix + PVC-survival make the controller path fully sufficient; no change needed.
4. **Fast-fail on `InvalidConfiguration`** (small): broker can stop waiting for `claim_timeout_seconds`
   when claim reports `Ready=False reason=InvalidConfiguration|PodFailed` (v1.0.3+/SDK parity).
5. **`status.sandbox.serviceFQDN`** (small, optional): could replace our podIP handling if we ever
   route via the headless Service instead of pod-direct.
6. **Claim TTL instead of reaper for ephemeral** (small, optional): `spec.lifecycle.ttlSecondsAfterFinished`
   + `shutdownPolicy: Delete` moves reap-deletion into the controller — but our reaper also does S3
   offload first, so only usable for the no-S3 path.

## 7. Source index

- Releases: v0.5.6 https://github.com/kubernetes-sigs/agent-sandbox/releases/tag/v0.5.6 ;
  v1.0.0 https://github.com/kubernetes-sigs/agent-sandbox/releases/tag/v1.0.0 ; likewise /v1.0.1 … /v1.0.7
- Manifests: https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.7/sandbox-with-extensions.yaml
  (and /v0.5.6/) ; CRDs: `k8s/crds/*.yaml` at both tags ; upstream chart: `helm/values.yaml` @ v1.0.7
- Code (blob-sha-verified v1.0.7: `sandboxclaim_controller.go` = `29797d7f98b90792c407b0f0c35fc190d75b3b7d`):
  `api/v1beta1/sandbox_types.go`, `extensions/api/v1beta1/sandboxclaim_types.go`,
  `extensions/api/v1beta1/sandboxtemplate_types.go`, `extensions/api/v1beta1/sandboxwarmpool_types.go`,
  `controllers/sandbox_controller.go`, `extensions/controllers/sandboxclaim_controller.go`,
  `extensions/controllers/sandboxwarmpool_controller.go`, `internal/lifecycle/expiry.go`
- Our context: `rust/shared/src/crd.rs`, `rust/broker/src/resolve.rs`, `rust/broker/src/sandbox.rs`,
  `open-websandbox-platform/upstream/` (SHA256SUMS verified), `open-websandbox-platform/chart/`.
