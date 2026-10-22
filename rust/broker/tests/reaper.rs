//! PR-C-3 integration: the idle reaper against the in-memory `StubSandboxStore`.
//!
//! Exercises the full `reap_once` tick (list → decide → act) without a cluster:
//! ephemeral→delete, persistent→park, persistent→reap, under-threshold→skip,
//! no-last-used→skip, the leader-gate skip on non-leaders, and the C-4 offload
//! keep-alive contract (a failing offload defers the delete). The pure idle
//! policy (`decide`) is unit-tested in `broker/src/reaper.rs`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use broker::reaper::{maybe_reap_once, reap_once, NoopOffload, OffloadError, ReapOffload};
use broker::test_fakes::StubSandboxStore;
use broker::{LeaderGate, SandboxStore};
use shared::{OperatingMode, Sandbox, SandboxSpec, ShutdownPolicy};

/// Build a broker-owned seed sandbox with the given profile / last-used / mode.
fn seed(name: &str, profile: &str, last_used: i64, mode: Option<OperatingMode>) -> Sandbox {
    let mut sbx = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: mode,
            shutdown_policy: Some(ShutdownPolicy::Retain),
            pod_template: None,
        },
    );
    sbx.metadata.namespace = Some("agent-sandbox-runtime".into());
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "owui-broker".to_string(),
    );
    labels.insert("broker-profile".to_string(), profile.to_string());
    sbx.metadata.labels = Some(labels);
    let mut annots = BTreeMap::new();
    annots.insert("broker-last-used".to_string(), last_used.to_string());
    annots.insert("broker-user".to_string(), "u".into());
    annots.insert("broker-session".to_string(), "s".into());
    sbx.metadata.annotations = Some(annots);
    sbx
}

/// Reaper config with small, memorable thresholds.
fn cfg(idle: u64, park: u64, reap: u64) -> shared::BrokerConfig {
    shared::BrokerConfig {
        idle_ttl_seconds: idle,
        park_idle_seconds: park,
        reap_seconds: reap,
        ..Default::default()
    }
}

/// A seed sandbox missing its `broker-last-used` annotation.
fn seed_no_last_used(name: &str, profile: &str) -> Sandbox {
    let mut sbx = seed(name, profile, 0, Some(OperatingMode::Running));
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .remove("broker-last-used");
    sbx
}

const NOW: i64 = 1_700_000_000;

#[tokio::test]
async fn ephemeral_over_idle_ttl_is_reaped_and_deleted() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-eph-1",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.reaped, 1);
    assert_eq!(stats.parked, 0);
    assert!(
        !store.snapshot().contains_key("owui-eph-1"),
        "reaped ephemeral is deleted from the store"
    );
}

#[tokio::test]
async fn ephemeral_under_idle_ttl_is_skipped() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-eph-2",
        "ephemeral",
        NOW - 10,
        Some(OperatingMode::Running),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.reaped, 0);
    assert!(store.snapshot().contains_key("owui-eph-2"));
}

#[tokio::test]
async fn persistent_over_park_ttl_is_parked_to_suspended() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-per-1",
        "persistent",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.parked, 1);
    assert_eq!(stats.reaped, 0);
    assert_eq!(
        store.snapshot()["owui-per-1"].spec.operating_mode,
        Some(OperatingMode::Suspended),
        "parked sandbox flipped to Suspended (object retained)"
    );
}

#[tokio::test]
async fn persistent_already_suspended_is_not_re_parked() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-per-2",
        "persistent",
        NOW - 200,
        Some(OperatingMode::Suspended),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats.parked, 0,
        "an already-Suspended sandbox is left alone"
    );
    assert_eq!(stats.skipped, 1);
    assert_eq!(
        store.snapshot()["owui-per-2"].spec.operating_mode,
        Some(OperatingMode::Suspended)
    );
}

#[tokio::test]
async fn persistent_over_reap_ttl_is_reaped_even_when_suspended() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-per-3",
        "persistent",
        NOW - 700_000,
        Some(OperatingMode::Suspended),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats.reaped, 1,
        "reap takes precedence over park, incl. parked"
    );
    assert!(!store.snapshot().contains_key("owui-per-3"));
}

#[tokio::test]
async fn sandbox_without_last_used_is_skipped() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_no_last_used("owui-eph-3", "ephemeral"));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.reaped, 0);
    assert!(store.snapshot().contains_key("owui-eph-3"));
}

#[tokio::test]
async fn mixed_batch_applies_per_sandbox_action() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "e-warm",
        "ephemeral",
        NOW - 10,
        Some(OperatingMode::Running),
    ));
    store.insert_sandbox(seed(
        "e-idle",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    store.insert_sandbox(seed(
        "p-warm",
        "persistent",
        NOW - 10,
        Some(OperatingMode::Running),
    ));
    store.insert_sandbox(seed(
        "p-park",
        "persistent",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    store.insert_sandbox(seed(
        "p-suspended",
        "persistent",
        NOW - 200,
        Some(OperatingMode::Suspended),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.scanned, 5);
    assert_eq!(stats.reaped, 1, "e-idle reaped");
    assert_eq!(stats.parked, 1, "p-park parked");
    assert_eq!(stats.skipped, 3, "e-warm + p-warm + p-suspended untouched");
    let snap = store.snapshot();
    assert!(!snap.contains_key("e-idle"));
    assert_eq!(
        snap["p-park"].spec.operating_mode,
        Some(OperatingMode::Suspended)
    );
    assert!(snap.contains_key("e-warm"));
    assert!(snap.contains_key("p-warm"));
    assert_eq!(
        snap["p-suspended"].spec.operating_mode,
        Some(OperatingMode::Suspended)
    );
}

#[tokio::test]
async fn non_leader_gate_skips_reaping_entirely() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-eph-9",
        "ephemeral",
        NOW - 999_999,
        Some(OperatingMode::Running),
    ));
    let c = cfg(1, 1, 1);
    let gate = LeaderGate::new(); // not leader

    let stats = maybe_reap_once(&gate, &store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats,
        broker::reaper::ReaperStats::default(),
        "a non-leader tick mutates nothing (no list, no delete, no park)"
    );
    assert!(
        store.snapshot().contains_key("owui-eph-9"),
        "idle sandbox survives a non-leader tick"
    );
}

// --- C-4 offload keep-alive contract -----------------------------------------

/// A stand-in for C-4's S3 offload that always fails — proves the reaper defers
/// the delete (D7: never silently lose a snapshot) and retries next tick.
struct FailingOffload;

#[async_trait]
impl ReapOffload for FailingOffload {
    async fn offload_on_reap(&self, _: &Sandbox) -> Result<(), OffloadError> {
        Err(OffloadError::Failed("simulated C-4 offload failure".into()))
    }
}

#[tokio::test]
async fn offload_failure_keeps_sandbox_alive_for_next_tick() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-eph-7",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(&store, &FailingOffload, &c, NOW).await;

    assert_eq!(stats.offload_failed, 1, "offload failure is counted");
    assert_eq!(stats.reaped, 0, "the delete is skipped on offload failure");
    assert!(
        store.snapshot().contains_key("owui-eph-7"),
        "keep-alive: the sandbox survives to be retried next tick"
    );
}

#[tokio::test]
async fn offload_success_then_delete_uses_default_noop() {
    // NoopOffload always succeeds → the delete proceeds (C-3 default behaviour).
    let store = Arc::new(StubSandboxStore::new());
    store.insert_sandbox(seed(
        "owui-eph-8",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    ));
    let store_dyn: Arc<dyn SandboxStore> = store.clone();
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(store_dyn.as_ref(), &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 1);
    assert!(!store.snapshot().contains_key("owui-eph-8"));
}

#[tokio::test]
async fn claimed_ephemeral_reap_deletes_claim_not_sandbox_directly() {
    let store = Arc::new(StubSandboxStore::new());
    let mut sbx = seed(
        "owui-warm000001",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    );
    // Claimed stamp: the reaper must delete the CLAIM (cascade), not the
    // Sandbox object directly.
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert("broker-claim".to_string(), "owui-claim1".to_string());
    store.insert_sandbox(sbx);
    let claim = shared::SandboxClaim::new(
        "owui-claim1",
        shared::SandboxClaimSpec {
            warm_pool_ref: shared::SandboxPoolRef {
                name: "code-standard-warmpool".into(),
            },
            additional_pod_metadata: None,
        },
    );
    store.insert_claim(claim);
    let store_dyn: Arc<dyn SandboxStore> = store.clone();
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(store_dyn.as_ref(), &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 1);
    assert!(
        !store.snapshot_claims().contains_key("owui-claim1"),
        "the claim is deleted"
    );
    assert!(
        !store.snapshot().contains_key("owui-warm000001"),
        "the bound sandbox cascade-deletes with the claim stub"
    );
}

#[tokio::test]
async fn claimed_ephemeral_reap_claim_gone_falls_back_to_sandbox() {
    let store = Arc::new(StubSandboxStore::new());
    let mut sbx = seed(
        "owui-warm000002",
        "ephemeral",
        NOW - 200,
        Some(OperatingMode::Running),
    );
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert("broker-claim".to_string(), "owui-vanished".to_string());
    store.insert_sandbox(sbx);
    // NOTE: no claim seeded — the reaper's claim delete 404s and falls back.
    let store_dyn: Arc<dyn SandboxStore> = store.clone();
    let c = cfg(120, 120, 604_800);

    let stats = reap_once(store_dyn.as_ref(), &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 1);
    assert!(!store.snapshot().contains_key("owui-warm000002"));
}

// --- #207 s3-record: suspended sandboxes are reap candidates ----------------

/// Reaper config in s3-record mode (S3 store of record; suspend = snapshot
/// + release).
fn cfg_s3_record(idle: u64, park: u64, reap: u64) -> shared::BrokerConfig {
    shared::BrokerConfig {
        idle_ttl_seconds: idle,
        park_idle_seconds: park,
        reap_seconds: reap,
        s3_enabled: true,
        persistent_mode: shared::PersistentMode::S3Record,
        ..Default::default()
    }
}

#[tokio::test]
async fn s3_record_reaps_suspended_sandbox_past_park_window() {
    // A Suspended s3-record sandbox pins its scratch storage (or a warm-pool
    // member) — past the park window it is released like any other idle
    // sandbox instead of being skipped forever.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-rec-1",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    ));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats.reaped, 1,
        "suspended s3-record sandbox past the park window is released"
    );
    assert_eq!(stats.skipped, 0);
    assert!(
        !store.snapshot().contains_key("owui-rec-1"),
        "released from the store"
    );
}

#[tokio::test]
async fn s3_record_suspended_without_offload_still_released_with_distinct_stat() {
    // The suspended sandbox cannot be offloaded (no serving pod, or an S3
    // outage) while a re-check still finds it Suspended — holding it
    // achieves nothing: the degraded case gets its own stat and the release
    // proceeds without the snapshot.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-rec-2",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    ));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &FailingOffload, &c, NOW).await;

    assert_eq!(
        stats.suspended_offload_unavailable, 1,
        "degraded suspend release surfaced distinctly"
    );
    assert_eq!(
        stats.offload_failed, 0,
        "the D7 keep-alive stat is NOT used for this case"
    );
    assert_eq!(stats.reaped, 1, "release proceeds without the offload");
    assert!(!store.snapshot().contains_key("owui-rec-2"));
}

#[tokio::test]
async fn per_user_pvc_leaves_suspended_sandboxes_untouched_even_with_s3_enabled() {
    // Mode gate: a PVC hot tier keeps the old contract — Suspended is a
    // valid parked state and the reaper skips it, even with the cold tier on.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-pvc-1",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    ));
    let c = shared::BrokerConfig {
        idle_ttl_seconds: 120,
        park_idle_seconds: 300,
        reap_seconds: 604_800,
        s3_enabled: true,
        persistent_mode: shared::PersistentMode::PerUserPvc,
        ..Default::default()
    };

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 0);
    assert_eq!(stats.skipped, 1, "suspended stays a parked state");
    assert_eq!(
        store.snapshot()["owui-pvc-1"].spec.operating_mode,
        Some(OperatingMode::Suspended)
    );
}

// --- #207 review fixes: gated release, decide() policy fold, release details ---

#[tokio::test]
async fn s3_record_running_member_past_park_window_is_reaped_not_parked() {
    // Composed pin for the decide() fold: a Running s3-record member past
    // the park window is reaped at park+1 — decide() alone is the policy
    // surface, no reap_once Park→Reap collapse.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed(
        "owui-rec-run",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Running),
    ));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.parked, 0, "s3-record never parks");
    assert_eq!(stats.reaped, 1, "Running member is reaped at park+1");
    assert!(!store.snapshot().contains_key("owui-rec-run"));
}

/// Simulates the P1-1 sequence: the offloader resumes the suspended sandbox
/// (patch `Running` + boot succeeds) and THEN fails its S3 put.
struct ResumeThenFailOffload {
    store: Arc<StubSandboxStore>,
}

#[async_trait]
impl ReapOffload for ResumeThenFailOffload {
    async fn offload_on_reap(&self, sbx: &Sandbox) -> Result<(), OffloadError> {
        let name = sbx.metadata.name.clone().unwrap_or_default();
        self.store
            .patch_operating_mode(&name, OperatingMode::Running)
            .await
            .expect("resume patch");
        Err(OffloadError::Failed("s3 put failed after resume".into()))
    }
}

#[tokio::test]
async fn s3_record_suspended_release_is_gated_when_offloader_resumed() {
    // The offloader resumed the sandbox before its S3 put failed — the
    // workspace is recoverable on the next tick, so the reaper must NOT
    // release it: ordinary D7 keep-alive, no release, no finalizer strip.
    let store = Arc::new(StubSandboxStore::new());
    let mut sbx = seed(
        "owui-rec-res",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    );
    sbx.metadata.finalizers = Some(vec![shared::FINALIZER_S3_SNAPSHOT.to_string()]);
    store.insert_sandbox(sbx);
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(
        store.as_ref(),
        &ResumeThenFailOffload {
            store: store.clone(),
        },
        &c,
        NOW,
    )
    .await;

    assert_eq!(
        stats.suspended_offload_unavailable, 0,
        "a resumed sandbox is NOT a degraded release"
    );
    assert_eq!(
        stats.offload_failed, 1,
        "resumed candidate keeps the ordinary D7 keep-alive stat"
    );
    assert_eq!(stats.reaped, 0, "a resumed sandbox is never released");
    let snap = store.snapshot();
    let kept = snap.get("owui-rec-res").expect("sandbox kept alive");
    assert_eq!(
        kept.spec.operating_mode,
        Some(OperatingMode::Running),
        "the offloader's resume stands"
    );
    assert!(
        store.finalizer_removals().is_empty(),
        "no finalizer strip on the keep-alive path"
    );
}

#[tokio::test]
async fn s3_record_suspended_bound_member_release_cascades_the_claim() {
    // The production stranded shape (#207): a claim-bound Suspended member.
    // The degraded release must delete the CLAIM (cascade frees the bound
    // Sandbox + pod and the pool replenishes), not the bare Sandbox.
    let store = Arc::new(StubSandboxStore::new());
    let mut sbx = seed(
        "owui-rec-bound",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    );
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert("broker-claim".to_string(), "owui-rec-claim".to_string());
    store.insert_sandbox(sbx);
    store.insert_claim(shared::SandboxClaim::new(
        "owui-rec-claim",
        shared::SandboxClaimSpec {
            warm_pool_ref: shared::SandboxPoolRef {
                name: "code-standard-record-pool".into(),
            },
            additional_pod_metadata: None,
        },
    ));
    let store_dyn: Arc<dyn SandboxStore> = store.clone();
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(store_dyn.as_ref(), &FailingOffload, &c, NOW).await;

    assert_eq!(stats.suspended_offload_unavailable, 1);
    assert_eq!(stats.reaped, 1);
    assert!(
        !store.snapshot_claims().contains_key("owui-rec-claim"),
        "the claim is deleted (cascade)"
    );
    assert!(
        !store.snapshot().contains_key("owui-rec-bound"),
        "the bound sandbox is cascade-released"
    );
}

#[tokio::test]
async fn s3_record_suspended_release_strips_finalizer_when_knob_active() {
    // The degraded release runs the release path's finalizer branch: with
    // the termination-finalizer knob active, the s3-snapshot finalizer is
    // stripped pre-delete so the released sandbox cannot linger Terminating
    // with no snapshot ever coming.
    let store = StubSandboxStore::new();
    let mut sbx = seed(
        "owui-rec-fin",
        "persistent",
        NOW - 400,
        Some(OperatingMode::Suspended),
    );
    sbx.metadata.finalizers = Some(vec![shared::FINALIZER_S3_SNAPSHOT.to_string()]);
    store.insert_sandbox(sbx);
    let c = cfg_s3_record(120, 300, 604_800);
    assert!(
        c.termination_finalizer_active(),
        "precondition: the termination-finalizer knob is active"
    );

    let stats = reap_once(&store, &FailingOffload, &c, NOW).await;

    assert_eq!(stats.suspended_offload_unavailable, 1);
    assert_eq!(stats.reaped, 1);
    assert!(!store.snapshot().contains_key("owui-rec-fin"));
    assert_eq!(
        store.finalizer_removals(),
        vec!["owui-rec-fin".to_string()],
        "the release path stripped the s3-snapshot finalizer"
    );
}

// --- #209: untracked (no broker-last-used) sandboxes age from creationTimestamp ---

/// A seed sandbox with NO `broker-last-used` (the claim-created shape: the
/// controller built the object and the best-effort touch never landed) whose
/// `metadata.creationTimestamp` is `created` epoch seconds.
fn seed_untracked_created_at(name: &str, profile: &str, created: i64) -> Sandbox {
    let mut sbx = seed_no_last_used(name, profile);
    sbx.metadata.creation_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        k8s_openapi::jiff::Timestamp::from_second(created).unwrap(),
    ));
    sbx
}

#[tokio::test]
async fn s3_record_sandbox_without_last_used_reaps_after_creation_timestamp_age() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_untracked_created_at(
        "owui-rec-nolu-1",
        "persistent",
        NOW - 400,
    ));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats.reaped, 1,
        "creationTimestamp age past the park window reaps instead of stranding forever"
    );
    assert!(!store.snapshot().contains_key("owui-rec-nolu-1"));
}

#[tokio::test]
async fn s3_record_sandbox_without_last_used_under_park_window_is_skipped() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_untracked_created_at(
        "owui-rec-nolu-2",
        "persistent",
        NOW - 10,
    ));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 0);
    assert_eq!(stats.skipped, 1, "a young untracked sandbox is still warm");
    assert!(store.snapshot().contains_key("owui-rec-nolu-2"));
}

#[tokio::test]
async fn s3_record_sandbox_without_last_used_nor_creation_timestamp_is_skipped() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_no_last_used("owui-rec-nolu-3", "persistent"));
    let c = cfg_s3_record(120, 300, 604_800);

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(
        stats.skipped, 1,
        "no aging input at all → conservative skip"
    );
    assert_eq!(stats.reaped, 0);
}

#[tokio::test]
async fn per_user_pvc_sandbox_without_last_used_still_skips_not_reaps() {
    // Mode gate unchanged: parked per-user PVCs are legitimate, so an
    // untracked PVC-mode sandbox keeps the "not yet tracked" skip.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_untracked_created_at(
        "owui-pvc-nolu-1",
        "persistent",
        NOW - 400,
    ));
    let c = shared::BrokerConfig {
        idle_ttl_seconds: 120,
        park_idle_seconds: 300,
        reap_seconds: 604_800,
        s3_enabled: true,
        persistent_mode: shared::PersistentMode::PerUserPvc,
        ..Default::default()
    };

    let stats = reap_once(&store, &NoopOffload, &c, NOW).await;

    assert_eq!(stats.reaped, 0);
    assert_eq!(stats.skipped, 1);
    assert!(store.snapshot().contains_key("owui-pvc-nolu-1"));
}
