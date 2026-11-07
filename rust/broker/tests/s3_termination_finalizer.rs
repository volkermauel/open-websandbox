//! s3-termination-finalizer integration: the reaper's `deletionTimestamp`
//! branch (snapshot contract) and the finalizer registration on the
//! claim-bind / cold-create resolve paths, exercised against the in-memory
//! `StubSandboxStore` — no live Kubernetes cluster required.

#![forbid(unsafe_code)]
// rust-stable 1.99 asserts these should be assert_eq; intentional here.
#![allow(clippy::assert_is_empty)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use broker::reaper::{reap_once, NoopOffload, OffloadError, ReapOffload};
use broker::s3::{s3_object_key, S3_LAST_KEY_ANNOTATION};
use broker::test_fakes::StubSandboxStore;
use broker::{resolve_sandbox, sandbox_name, AppState};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use shared::{
    BrokerConfig, OperatingMode, PersistentMode, Profile, Sandbox, SandboxCondition, SandboxSpec,
    SandboxStatus, ShutdownPolicy,
};

const FINALIZER: &str = shared::FINALIZER_S3_SNAPSHOT;
const NOW: i64 = 1_700_000_000;
const DELETED_AT: &str = "2023-11-14T22:13:20Z";

/// s3-record config with the termination finalizer on (the default).
fn cfg_s3_record() -> BrokerConfig {
    BrokerConfig {
        idle_ttl_seconds: 120,
        park_idle_seconds: 300,
        reap_seconds: 604_800,
        s3_enabled: true,
        persistent_mode: PersistentMode::S3Record,
        ..Default::default()
    }
}

/// Seed a broker-owned persistent sandbox with last-used / mode, then apply
/// `mutate` for the scenario-specific metadata (finalizer, stamp, deletion).
fn seed_with(name: &str, last_used: i64, mutate: impl FnOnce(&mut Sandbox)) -> Sandbox {
    let mut sbx = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: Some(OperatingMode::Running),
            shutdown_policy: Some(ShutdownPolicy::Retain),
            pod_template: None,
        },
    );
    sbx.metadata.namespace = Some("agent-sandbox-runtime".into());
    let mut labels = std::collections::BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "owui-broker".to_string(),
    );
    labels.insert("broker-profile".to_string(), "persistent".to_string());
    sbx.metadata.labels = Some(labels);
    let mut annots = std::collections::BTreeMap::new();
    annots.insert("broker-last-used".to_string(), last_used.to_string());
    annots.insert("broker-user".to_string(), "u".into());
    annots.insert("broker-session".to_string(), "s".into());
    sbx.metadata.annotations = Some(annots);
    mutate(&mut sbx);
    sbx
}

/// Mark a sandbox Terminating (as the apiserver does when a delete lands on
/// an object that still carries a finalizer).
fn terminating(sbx: &mut Sandbox) {
    // `Time` wraps a jiff Timestamp with no FromStr; go through serde.
    let t: Time = serde_json::from_str(&format!("\"{DELETED_AT}\"")).expect("timestamp");
    sbx.metadata.deletion_timestamp = Some(t);
}

fn with_finalizer(sbx: &mut Sandbox) {
    sbx.metadata
        .finalizers
        .get_or_insert_with(Vec::new)
        .push(FINALIZER.to_string());
}

/// Ready status carrying a pod IP — the "a pod exists" side of the
/// no-pod-vs-offload-failure distinction.
fn ready_status(ip: &str) -> SandboxStatus {
    SandboxStatus {
        phase: Some("Running".into()),
        pod_i_ps: Some(vec![ip.into()]),
        conditions: Some(vec![SandboxCondition {
            r#type: "Ready".into(),
            status: "True".into(),
            reason: None,
            message: None,
            last_transition_time: None,
        }]),
        ready: Some(true),
        message: None,
    }
}

/// Stamp the last-offloaded-key annotation the way `S3Offload` does after a
/// fully-successful offload (the ts is embedded in the key).
fn stamped(sbx: &mut Sandbox, ts: i64) {
    let key = s3_object_key("users", "u", "s", ts);
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(S3_LAST_KEY_ANNOTATION.to_string(), key);
}

fn has_finalizer(snap: &std::collections::HashMap<String, Sandbox>, name: &str) -> bool {
    snap.get(name)
        .and_then(|s| s.metadata.finalizers.as_ref())
        .is_some_and(|f| f.iter().any(|v| v == FINALIZER))
}

/// Records offload invocations and always succeeds.
struct CountingOffload {
    calls: AtomicUsize,
}

impl CountingOffload {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ReapOffload for CountingOffload {
    async fn offload_on_reap(&self, _: &Sandbox) -> Result<(), OffloadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Always fails — proves the finalizer holds the deletion (retry next tick).
struct FailingOffload;

#[async_trait]
impl ReapOffload for FailingOffload {
    async fn offload_on_reap(&self, _: &Sandbox) -> Result<(), OffloadError> {
        Err(OffloadError::Failed("simulated S3 outage".into()))
    }
}

// --- termination handling (deletionTimestamp branch) ------------------------

#[tokio::test]
async fn out_of_band_delete_archives_then_releases() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-ood1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
        // A serving pod existed when the out-of-band delete landed — without
        // a pod IP there is nothing to offload and the no-pod release fires
        // instead (see terminating_sandbox_without_a_pod_releases_the_finalizer).
        sbx.status = Some(ready_status("10.1.1.1"));
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(
        offload.calls(),
        1,
        "an out-of-band delete with no fresh stamp offloads exactly once"
    );
    assert_eq!(stats.termination_blocked, 0);
    let snap = store.snapshot();
    assert!(
        !has_finalizer(&snap, "owui-c-ood1"),
        "the finalizer is released after the offload"
    );
    assert!(
        snap.contains_key("owui-c-ood1"),
        "the stub does not complete the delete; the apiserver does"
    );
}

#[tokio::test]
async fn fresh_snapshot_stamp_dedupes_the_offload() {
    let store = StubSandboxStore::new();
    // Offload stamp NEWER than the last activity: the reaper already archived
    // the chat at its current snapshot generation (the common idle-reap
    // path), so the terminating tick must NOT offload again.
    store.insert_sandbox(seed_with("owui-c-dedupe", NOW - 1_000, |sbx| {
        stamped(sbx, NOW - 100);
        with_finalizer(sbx);
        terminating(sbx);
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(
        offload.calls(),
        0,
        "a fresh snapshot short-circuits the offload"
    );
    assert_eq!(stats.termination_blocked, 0);
    assert!(
        !has_finalizer(&store.snapshot(), "owui-c-dedupe"),
        "the finalizer is released without a second offload"
    );
}

#[tokio::test]
async fn stale_snapshot_stamp_offloads_anyway() {
    let store = StubSandboxStore::new();
    // Activity AFTER the archive: the stamp is stale, the workspace may hold
    // unarchived changes.
    store.insert_sandbox(seed_with("owui-c-stale1", NOW - 10, |sbx| {
        stamped(sbx, NOW - 5_000);
        with_finalizer(sbx);
        terminating(sbx);
        // A serving pod: the stale stamp means the workspace may hold
        // unarchived changes, so the offload still runs.
        sbx.status = Some(ready_status("10.1.1.2"));
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(offload.calls(), 1, "a stale stamp must not dedupe");
    assert_eq!(stats.termination_blocked, 0);
    assert!(!has_finalizer(&store.snapshot(), "owui-c-stale1"));
}

#[tokio::test]
async fn offload_failure_holds_the_finalizer() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-fail1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
        // A pod IP is present: the offload CAN run (a runtime could serve
        // the snapshot), so a failure genuinely holds (D7). A sandbox with
        // NO pod at all releases instead — see
        // terminating_sandbox_without_a_pod_releases_the_finalizer.
        sbx.status = Some(ready_status("10.1.1.3"));
    }));

    let stats = reap_once(&store, &FailingOffload, &cfg_s3_record(), NOW).await;

    assert_eq!(
        stats.termination_blocked, 1,
        "the blocked deletion is counted"
    );
    let snap = store.snapshot();
    assert!(
        has_finalizer(&snap, "owui-c-fail1"),
        "the finalizer stays: deletion waits for the snapshot"
    );
    assert!(
        snap.contains_key("owui-c-fail1"),
        "the sandbox is kept for the next tick's retry"
    );
}

/// 2026-11-06 incident: sandboxes pinned to a cordoned node never got a pod
/// (FailedScheduling), were deleted, and then sat Terminating forever — the
/// reaper retried the offload every tick and always failed with "no pod IP".
/// No pod ⇒ nothing to offload ⇒ the finalizer must be released.
#[tokio::test]
async fn terminating_sandbox_without_a_pod_releases_the_finalizer() {
    let store = StubSandboxStore::new();
    // Running mode, NO status, NO pod object: never scheduled.
    store.insert_sandbox(seed_with("owui-c-nopod1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(
        offload.calls(),
        0,
        "no pod ⇒ the offload must not even be attempted"
    );
    assert_eq!(stats.termination_blocked, 0, "nothing is blocked");
    assert_eq!(
        stats.termination_released_no_offload, 1,
        "the release-without-offload case is counted"
    );
    assert!(
        !has_finalizer(&store.snapshot(), "owui-c-nopod1"),
        "the finalizer is released so the apiserver can finish the delete"
    );
}

/// The D7 distinction: a pod IP EXISTS (a runtime could serve the snapshot)
/// but the offload call fails — THAT must keep holding, the no-pod release
/// must not fire.
#[tokio::test]
async fn terminating_sandbox_with_a_pod_but_failing_offload_still_holds() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-haspod1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
        sbx.status = Some(ready_status("10.9.9.9"));
    }));

    let stats = reap_once(&store, &FailingOffload, &cfg_s3_record(), NOW).await;

    assert_eq!(stats.termination_blocked, 1, "D7: a failing offload holds");
    assert_eq!(stats.termination_released_no_offload, 0);
    assert!(has_finalizer(&store.snapshot(), "owui-c-haspod1"));
}

/// #207 exemption: a SUSPENDED terminating sandbox may still be resumed by
/// the offloader to take the snapshot, so the no-pod release must NOT fire
/// for it even though it has no pod IP right now.
#[tokio::test]
async fn terminating_suspended_sandbox_keeps_the_resume_offload_contract() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-susp1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
        sbx.spec.operating_mode = Some(OperatingMode::Suspended);
    }));

    let stats = reap_once(&store, &FailingOffload, &cfg_s3_record(), NOW).await;

    assert_eq!(
        stats.termination_released_no_offload, 0,
        "never the no-pod release"
    );
    assert_eq!(stats.termination_blocked, 1);
    assert!(has_finalizer(&store.snapshot(), "owui-c-susp1"));
}

/// The WARN the on-call greps for: the release-without-offload log names the
/// sandbox and says WHY the finalizer went away without a snapshot.
#[tokio::test]
async fn no_pod_release_logs_a_warn_naming_the_sandbox_and_reason() {
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = SinkWriter;
        fn make_writer(&'a self) -> Self::Writer {
            SinkWriter(self.0.clone())
        }
    }
    struct SinkWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SinkWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = std::sync::Arc::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .with_writer(Sink(buf.clone()))
        .finish();
    // Scope the thread-local dispatcher over the WHOLE test body (guard held
    // across the await — #[tokio::test] is current-thread, so every emit and
    // the final read share one thread and one dispatcher).
    let _dispatch = tracing::subscriber::set_default(subscriber);

    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-log1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
    }));
    let stats = reap_once(&store, &CountingOffload::new(), &cfg_s3_record(), NOW).await;
    assert_eq!(stats.termination_released_no_offload, 1);

    let out = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8 log capture");
    assert!(
        out.contains("owui-c-log1"),
        "the WARN names the sandbox: {out}"
    );
    assert!(
        out.contains("releasing finalizer without a snapshot"),
        "the WARN states the release and its reason: {out}"
    );
    assert!(out.contains("no pod IP"), "the WARN says why: {out}");
    assert!(
        !out.contains("finalizer held"),
        "no misleading hold log: {out}"
    );
}

#[tokio::test]
async fn knob_off_releases_without_offloading() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-knob1", NOW - 10, |sbx| {
        stamped(sbx, NOW - 5_000);
        with_finalizer(sbx);
        terminating(sbx);
    }));
    let cfg = BrokerConfig {
        s3_termination_finalizer: false,
        ..cfg_s3_record()
    };
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg, NOW).await;

    assert_eq!(
        offload.calls(),
        0,
        "knob off: never offload on the termination path"
    );
    assert!(
        !has_finalizer(&store.snapshot(), "owui-c-knob1"),
        "knob off: never block a deletion"
    );
    assert_eq!(stats.termination_blocked, 0);
}

#[tokio::test]
async fn non_s3_modes_never_block_deletion() {
    // per-user-pvc (the legacy store of record): a terminating sandbox with a
    // leftover finalizer is released untouched — no offload, no block.
    for mode in [PersistentMode::PerUserPvc, PersistentMode::EmptyDir] {
        let store = StubSandboxStore::new();
        store.insert_sandbox(seed_with("owui-c-mode1", NOW - 10, |sbx| {
            stamped(sbx, NOW - 5_000);
            with_finalizer(sbx);
            terminating(sbx);
        }));
        let cfg = BrokerConfig {
            s3_enabled: true,
            persistent_mode: mode,
            ..Default::default()
        };
        let offload = CountingOffload::new();

        let stats = reap_once(&store, &offload, &cfg, NOW).await;

        assert_eq!(offload.calls(), 0, "{mode:?}: no termination offload");
        assert!(!has_finalizer(&store.snapshot(), "owui-c-mode1"));
        assert_eq!(stats.termination_blocked, 0, "{mode:?}");
    }
}

#[tokio::test]
async fn terminating_sandbox_without_our_finalizer_is_untouched() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-other", NOW - 10, |sbx| {
        terminating(sbx);
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(offload.calls(), 0);
    assert_eq!(stats.termination_blocked, 0);
}

#[tokio::test]
async fn reap_of_a_finalized_sandbox_releases_before_the_delete() {
    // s3-record idle reap (park collapses to reap): the reaper offloads,
    // releases the finalizer, then deletes — the release must not error or
    // count the sandbox as blocked.
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-reap1", NOW - 400, |sbx| {
        with_finalizer(sbx);
    }));
    let offload = CountingOffload::new();

    let stats = reap_once(&store, &offload, &cfg_s3_record(), NOW).await;

    assert_eq!(stats.reaped, 1);
    assert_eq!(offload.calls(), 1);
    assert_eq!(stats.termination_blocked, 0);
    assert!(
        !store.snapshot().contains_key("owui-c-reap1"),
        "the reaper-initiated delete completes in the same tick"
    );
}

// --- finalizer registration (resolve paths) ----------------------------------

fn resolve_template() -> shared::SandboxTemplate {
    shared::SandboxTemplate::new(
        "code-standard-v1",
        shared::SandboxTemplateSpec {
            description: None,
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "code-standard:latest",
                        "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]
                    }],
                    "volumes": [{"name": "workspace", "emptyDir": {}}]
                }
            })),
        },
    )
}

#[tokio::test]
async fn cold_create_in_s3_record_carries_the_finalizer() {
    let store = std::sync::Arc::new(StubSandboxStore::new());
    store.insert_template(resolve_template());
    store.set_auto_ready_on_create(Some("10.0.0.5".into()));
    // No pools configured → the direct cold-create path.
    let cfg = BrokerConfig {
        warm_pool: String::new(),
        claim_timeout_seconds: 5,
        ..cfg_s3_record()
    };
    let state = AppState::new(cfg, store.clone());

    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("s3-record cold create resolves");

    let snap = store.snapshot();
    let sbx = snap.get(&resolved.name).expect("sandbox");
    assert!(
        sbx.metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| f.iter().any(|v| v == FINALIZER)),
        "cold-created s3-record sandboxes carry the snapshot finalizer"
    );
}

#[tokio::test]
async fn cold_create_outside_s3_record_never_carries_the_finalizer() {
    let store = std::sync::Arc::new(StubSandboxStore::new());
    store.insert_template(resolve_template());
    store.set_auto_ready_on_create(Some("10.0.0.6".into()));
    let cfg = BrokerConfig {
        shared_secret: "test-broker-shared-secret-0123456789".into(),
        claim_timeout_seconds: 5,
        ..Default::default()
    };
    let state = AppState::new(cfg, store.clone());

    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("per-user-pvc cold create resolves");

    let snap = store.snapshot();
    let sbx = snap.get(&resolved.name).expect("sandbox");
    assert!(
        sbx.metadata.finalizers.as_ref().is_none_or(Vec::is_empty),
        "per-user-pvc never registers the snapshot finalizer"
    );
}

#[tokio::test]
async fn claim_bind_in_s3_record_stamps_the_finalizer() {
    let store = std::sync::Arc::new(StubSandboxStore::new());
    store.insert_template(resolve_template());
    store.set_pool_key("pool-key-123");
    let cfg = BrokerConfig {
        warm_pool_record: "record-pool".into(),
        ..cfg_s3_record()
    };
    let state = AppState::new(cfg, store.clone());

    // Simulate the controller binding the claim to a warm pool member.
    let s2 = store.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let claims = s2.snapshot_claims();
        let (claim_name, _) = claims.into_iter().next().expect("claim created");
        let bound = "owui-rec0000042";
        s2.mark_claim_bound(&claim_name, bound);
        s2.insert_sandbox(Sandbox::new(
            bound,
            SandboxSpec {
                template_name: None,
                operating_mode: None,
                shutdown_policy: None,
                pod_template: None,
            },
        ));
        s2.mark_pod_ready(bound, "10.1.1.9");
    });

    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("s3-record claim binds + resolves");

    let snap = store.snapshot();
    let sbx = snap.get(&resolved.name).expect("bound sandbox");
    assert_eq!(
        sbx.metadata
            .annotations
            .as_ref()
            .expect("annotations")
            .get("broker-claim"),
        Some(&sandbox_name("user-1", "chat-1", Profile::Persistent)),
        "precondition: the broker stamped the bound sandbox"
    );
    assert!(
        sbx.metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| f.iter().any(|v| v == FINALIZER)),
        "claim-bound s3-record sandboxes carry the snapshot finalizer"
    );
}

// Keep the no-op import honest: the NoopOffload path is the C-3 default and
// exercises the knob-off flow identically (always-Ok offload → release).
#[tokio::test]
async fn noop_offload_on_terminating_sandbox_releases_immediately() {
    let store = StubSandboxStore::new();
    store.insert_sandbox(seed_with("owui-c-noop1", NOW - 10, |sbx| {
        with_finalizer(sbx);
        terminating(sbx);
    }));

    let stats = reap_once(&store, &NoopOffload, &cfg_s3_record(), NOW).await;

    assert_eq!(stats.termination_blocked, 0);
    assert!(
        !has_finalizer(&store.snapshot(), "owui-c-noop1"),
        "a no-op offload never blocks deletion"
    );
}
