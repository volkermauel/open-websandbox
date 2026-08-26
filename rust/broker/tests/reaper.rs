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
