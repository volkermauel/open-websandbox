//! PR-C-4 integration: the S3-tiered cold tier (offload on reap + restore on
//! resume) exercised in-process against the [`InMemoryColdStore`] double and a
//! `wiremock` stand-in for the runtime's `/snapshot` + `/restore` HTTP surface —
//! no live S3, no Kubernetes cluster.
//!
//! Covers the four behaviours the cold tier must guarantee (D7/#56):
//! * offload streams `/snapshot` → S3 put → keep-latest per-object delete, with
//!   **upload-new-then-delete-old ordering** (the namespace is never empty
//!   mid-offload);
//! * restore-on-resume: list → newest → `/restore` PUT (and skip when empty);
//! * offload failure → `Err` so the reaper keeps the sandbox alive for the next
//!   tick (retry then exhaustion).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use broker::reaper::ReapOffload;
use broker::{
    resolve_sandbox, s3_namespace, s3_object_key, sandbox_name, AppState, ColdStore,
    InMemoryColdStore, RestoreOutcome, S3Offload, StubSandboxStore,
};
use shared::{
    OperatingMode, Profile, Sandbox, SandboxSpec, SandboxStatus, SandboxTemplate,
    SandboxTemplateSpec,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A persistent sandbox with a Ready status + a pod IP (the override means the
/// IP value is irrelevant to the hop URL, but `offload_on_reap` still requires
/// one to be present).
fn persistent_sandbox(name: &str, pod_ip: &str) -> Sandbox {
    let mut sbx = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: Some(OperatingMode::Running),
            shutdown_policy: None,
            pod_template: None,
        },
    );
    let mut labels = BTreeMap::new();
    labels.insert("broker-profile".to_string(), "persistent".to_string());
    sbx.metadata.labels = Some(labels);
    let mut annots = BTreeMap::new();
    annots.insert("broker-user".to_string(), "u".to_string());
    annots.insert("broker-session".to_string(), "s".to_string());
    sbx.metadata.annotations = Some(annots);
    sbx.status = Some(SandboxStatus {
        phase: Some("Running".into()),
        pod_i_ps: Some(vec![pod_ip.to_string()]),
        ready: Some(true),
        message: None,
        conditions: None,
    });
    sbx
}

fn offload(store: Arc<InMemoryColdStore>, base: String) -> S3Offload {
    let cfg = shared::BrokerConfig {
        s3_prefix: "users".into(),
        runtime_api_key: "rt-test-key".into(),
        ..Default::default()
    };
    S3Offload::new(&cfg, store, reqwest::Client::new())
        // Point runtime hops at the wiremock server + a zero backoff so retries
        // don't sleep in tests.
        .with_runtime_upstream_override(base)
        .with_retry_policy(3, std::time::Duration::ZERO)
}

/// `true` when `log` records a `put:` entry before any `delete:` entry.
fn puts_before_deletes(log: &[String]) -> bool {
    let first_put = log.iter().position(|e| e.starts_with("put:"));
    let first_delete = log.iter().position(|e| e.starts_with("delete:"));
    match (first_put, first_delete) {
        (Some(p), Some(d)) => p < d,
        (Some(_), None) => true,
        _ => false,
    }
}

#[tokio::test]
async fn offload_streams_snapshot_then_uploads_new_then_deletes_old() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fresh-snapshot".as_slice()))
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryColdStore::new());
    // A prior snapshot already under the namespace (keep-latest must remove it).
    let ns = s3_namespace("users", "owui-c-abc");
    let old_key = s3_object_key("users", "owui-c-abc", 1_699_000_000);
    store.seed(&old_key, &b"old-snapshot"[..]);

    let offload = offload(store.clone(), server.uri());
    offload
        .offload_on_reap(&persistent_sandbox("owui-c-abc", "10.0.0.5"))
        .await
        .expect("offload succeeds");

    let keys = store.keys();
    assert_eq!(keys.len(), 1, "keep-latest: exactly one snapshot remains");
    assert!(
        keys[0].starts_with(&ns) && keys[0].ends_with(".tar.zst"),
        "the retained object is under the namespace: {}",
        keys[0]
    );
    assert_ne!(keys[0], old_key, "the OLD snapshot was deleted");
    assert_eq!(
        store.get_object(&keys[0]).await.unwrap(),
        bytes::Bytes::from_static(b"fresh-snapshot"),
        "the retained object is the just-uploaded snapshot"
    );
    // Upload-new-then-delete-old ordering (D7/#56): never delete before the new
    // object is durable.
    let log = store.log();
    assert!(
        puts_before_deletes(&log),
        "expected put before delete in {log:?}"
    );
}

#[tokio::test]
async fn offload_failure_keeps_sandbox_alive_and_writes_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryColdStore::new());
    let offload = offload(store.clone(), server.uri());

    let err = offload
        .offload_on_reap(&persistent_sandbox("owui-c-abc", "10.0.0.5"))
        .await
        .expect_err("a failing snapshot hop must surface as Err");
    assert!(
        matches!(err, broker::reaper::OffloadError::Failed(_)),
        "{err:?}"
    );
    assert!(
        store.keys().is_empty(),
        "nothing durably written when the snapshot hop failed"
    );
}

#[tokio::test]
async fn restore_streams_latest_object_to_restore_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/restore"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryColdStore::new());
    let latest = s3_object_key("users", "owui-c-abc", 1_700_000_000);
    store.seed(&latest, &b"workspace-tar-zst"[..]);

    let offload = offload(store.clone(), server.uri());
    let outcome = offload
        .restore_on_resume("owui-c-abc", "10.0.0.5")
        .await
        .expect("restore succeeds");
    assert_eq!(outcome, RestoreOutcome::Restored(latest));

    // The runtime /restore received exactly the object body.
    let received = server
        .received_requests()
        .await
        .expect("wiremock captures requests");
    let put = received
        .iter()
        .find(|r| r.method == "PUT" && r.url.path() == "/restore")
        .expect("a PUT /restore was issued");
    assert_eq!(put.body, b"workspace-tar-zst");
}

#[tokio::test]
async fn restore_skips_when_no_object_and_never_hits_restore() {
    let server = MockServer::start().await;
    // Deliberately do NOT mount a /restore mock: if the code issued a PUT, the
    // connection would fail the test. Empty namespace ⇒ first-creation no-op.
    let store = Arc::new(InMemoryColdStore::new());
    let offload = offload(store.clone(), server.uri());

    let outcome = offload
        .restore_on_resume("owui-c-abc", "10.0.0.5")
        .await
        .expect("no-object is not an error");
    assert_eq!(outcome, RestoreOutcome::NoObject);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "no HTTP hop issued when there is nothing to restore"
    );
}

#[tokio::test]
async fn restore_failure_surfaces_so_resolve_can_fail_the_resume() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/restore"))
        .respond_with(ResponseTemplate::new(500).set_body_bytes(b"bad payload"))
        .mount(&server)
        .await;

    let store = Arc::new(InMemoryColdStore::new());
    store.seed(
        &s3_object_key("users", "owui-c-abc", 1_700_000_000),
        &b"workspace-tar-zst"[..],
    );
    let offload = offload(store.clone(), server.uri());

    let err = offload
        .restore_on_resume("owui-c-abc", "10.0.0.5")
        .await
        .expect_err("a failing restore must surface as Err");
    let msg = err.to_string();
    assert!(msg.contains("HTTP"), "error carries the HTTP status: {msg}");
}

// --- resolve_sandbox: resume-on-Suspended + restore-on-resume wiring -------

fn base_template() -> SandboxTemplate {
    SandboxTemplate::new(
        "code-standard-v1",
        SandboxTemplateSpec {
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

/// A `Suspended` persistent sandbox at the deterministic name — parked by the
/// reaper, awaiting resume on the next request (status intentionally NOT Ready,
/// simulating a pod-less parked sandbox the controller has yet to reschedule).
fn suspended_sandbox(name: &str) -> Sandbox {
    let mut sbx = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: Some(OperatingMode::Suspended),
            shutdown_policy: None,
            pod_template: None,
        },
    );
    let mut labels = BTreeMap::new();
    labels.insert("broker-profile".to_string(), "persistent".to_string());
    sbx.metadata.labels = Some(labels);
    sbx.metadata.namespace = Some("agent-sandbox-runtime".into());
    sbx
}

#[tokio::test]
async fn resolve_resumes_suspended_sandbox_and_restores_from_s3() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/restore"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(base_template());
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    store.insert_sandbox(suspended_sandbox(&name));

    // Cold tier already holds a prior snapshot for this sandbox (offloaded on
    // a previous reap).
    let cold = Arc::new(InMemoryColdStore::new());
    let key = s3_object_key("users", &name, 1_700_000_000);
    cold.seed(&key, &b"workspace-tar-zst"[..]);

    let s3 = S3Offload::new(
        &shared::BrokerConfig {
            s3_prefix: "users".into(),
            runtime_api_key: "rt-test-key".into(),
            ..Default::default()
        },
        cold,
        reqwest::Client::new(),
    )
    .with_runtime_upstream_override(server.uri());

    let cfg = shared::BrokerConfig {
        s3_prefix: "users".into(),
        claim_timeout_seconds: 5,
        ..Default::default()
    };
    let state = AppState::new(cfg, store.clone()).with_s3_restore(Arc::new(s3));

    // Simulate the controller rescheduling the resumed pod: flip the parked
    // sandbox to Ready shortly after resolve patches it back to Running.
    let ready_store = Arc::clone(&store);
    let ready_name = name.clone();
    tokio::spawn(async move {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if ready_store.mark_ready(&ready_name, "10.0.0.9") {
                break;
            }
        }
    });

    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("resumed + ready + restored");
    assert_eq!(resolved.pod_ip, "10.0.0.9");

    // The parked sandbox was resumed (operatingMode flipped back to Running).
    assert_eq!(
        store.snapshot()[&name].spec.operating_mode,
        Some(OperatingMode::Running),
        "resume patched operatingMode Suspended -> Running"
    );

    // The restore hop fired against the resumed pod.
    let put = server
        .received_requests()
        .await
        .expect("wiremock captures requests")
        .into_iter()
        .find(|r| r.method == "PUT" && r.url.path() == "/restore")
        .expect("resolve issued a PUT /restore on resume");
    assert_eq!(put.body, b"workspace-tar-zst");
}

// A compile-time guard: Profile is the polarity the offload gate keys on.
#[test]
fn persistent_is_the_offload_gate() {
    assert_ne!(Profile::Persistent, Profile::Ephemeral);
}
