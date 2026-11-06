//! A) Fast-fail on terminal unschedulability: a pod (or its Sandbox-status
//! mirror) reporting `PodScheduled=False reason=Unschedulable` must 503
//! IMMEDIATELY with the condition message — not after polling out the full
//! claim timeout (the 2026-11-06 incident: a cordoned node pinned by the
//! template made every resolve hang 60s and look "slow" instead of failing
//! with the reason). `SchedulingGated` is explicitly NOT terminal: it keeps
//! the normal wait/timeout (gates can be lifted).

#![forbid(unsafe_code)]
// rust-stable 1.99 asserts these should be assert_eq; intentional here.
#![allow(clippy::assert_is_empty)]
use std::sync::Arc;
use std::time::Instant;

use broker::test_fakes::StubSandboxStore;
use broker::{resolve_sandbox, sandbox_name, ApiError, AppState};
use shared::{BrokerConfig, Profile, Sandbox, SandboxSpec, SandboxTemplate, SandboxTemplateSpec};

const BASE_TEMPLATE: &str = "code-standard-v1";

fn template(image: &str) -> SandboxTemplate {
    SandboxTemplate::new(
        BASE_TEMPLATE,
        SandboxTemplateSpec {
            description: None,
            pod_template: Some(serde_json::json!({
                "spec": {
                    "containers": [{"name": "sandbox", "image": image}],
                    "volumes": [{"name": "workspace", "emptyDir": {}}]
                }
            })),
        },
    )
}

fn config(timeout_secs: u64) -> BrokerConfig {
    BrokerConfig {
        shared_secret: "test-broker-shared-secret-0123456789".into(),
        runtime_ns: "agent-sandbox-runtime".into(),
        base_template: BASE_TEMPLATE.into(),
        runtime_api_key: "rt-test-key".into(),
        claim_timeout_seconds: timeout_secs,
        ..BrokerConfig::default()
    }
}

/// A bare pod with one condition (`Pending`, no IP) — never Ready.
fn pending_pod(
    name: &str,
    cond_type: &str,
    reason: &str,
    message: &str,
) -> k8s_openapi::api::core::v1::Pod {
    k8s_openapi::api::core::v1::Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        status: Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Pending".into()),
            conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                type_: cond_type.to_string(),
                status: "False".to_string(),
                reason: Some(reason.to_string()),
                message: Some(message.to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Sandbox status carrying a mirrored `PodScheduled=False` condition.
fn mirrored_unschedulable_status(message: &str) -> shared::SandboxStatus {
    shared::SandboxStatus {
        phase: Some("Pending".into()),
        conditions: Some(vec![shared::SandboxCondition {
            r#type: "PodScheduled".into(),
            status: "False".into(),
            reason: Some("Unschedulable".into()),
            message: Some(message.into()),
            last_transition_time: None,
        }]),
        ..Default::default()
    }
}

/// An existing, broker-owned sandbox with no pod and no status (nothing ever
/// schedules it) — the shape the incident produced.
fn stuck_sandbox(name: &str) -> Sandbox {
    let mut sbx = Sandbox::new(name, SandboxSpec::default());
    sbx.metadata.namespace = Some("agent-sandbox-runtime".into());
    sbx.metadata.labels = Some(
        [(
            "app.kubernetes.io/managed-by".to_string(),
            "owui-broker".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    sbx
}

#[tokio::test]
async fn unschedulable_pod_condition_fails_fast_with_the_reason() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:latest"));
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    store.insert_sandbox(stuck_sandbox(&name));
    store.put_pod(pending_pod(
        &name,
        "PodScheduled",
        "Unschedulable",
        "0/11 nodes are available: 1 node(s) were unschedulable.",
    ));

    let state = AppState::new(config(10), store.clone());
    let start = Instant::now();
    let err = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect_err("unschedulable must fail fast");
    let elapsed = start.elapsed();

    match &err {
        ApiError::ServiceUnavailable(msg) => {
            assert!(msg.contains("unschedulable"), "{msg}");
            assert!(
                msg.contains("0/11 nodes are available"),
                "503 must carry the condition message: {msg}"
            );
        }
        other => panic!("expected 503 ServiceUnavailable, got {other:?}"),
    }
    // The whole point: do NOT poll out the 10s claim timeout.
    assert!(
        elapsed.as_secs() < 5,
        "fast-fail took {elapsed:?} — it must not wait the claim timeout"
    );
}

#[tokio::test]
async fn unschedulable_sandbox_status_mirror_fails_fast_too() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:latest"));
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut sbx = stuck_sandbox(&name);
    sbx.status = Some(mirrored_unschedulable_status(
        "0/11 nodes are available: 11 node(s) didn't match Pod's node affinity/selector.",
    ));
    store.insert_sandbox(sbx);

    let state = AppState::new(config(10), store.clone());
    let start = Instant::now();
    let err = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect_err("unschedulable mirror must fail fast");
    let elapsed = start.elapsed();

    match &err {
        ApiError::ServiceUnavailable(msg) => {
            assert!(
                msg.contains("node affinity/selector"),
                "503 must carry the mirrored condition message: {msg}"
            );
        }
        other => panic!("expected 503 ServiceUnavailable, got {other:?}"),
    }
    assert!(elapsed.as_secs() < 5, "fast-fail took {elapsed:?}");
}

#[tokio::test]
async fn scheduling_gated_is_not_terminal_and_times_out_normally() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:latest"));
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    store.insert_sandbox(stuck_sandbox(&name));
    store.put_pod(pending_pod(
        &name,
        "PodScheduled",
        "SchedulingGated",
        "not gated",
    ));

    let state = AppState::new(config(1), store.clone());
    let start = Instant::now();
    let err = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect_err("gated pod must keep the wait/timeout contract");
    let elapsed = start.elapsed();

    match &err {
        ApiError::ServiceUnavailable(msg) => {
            assert!(
                msg.contains("not ready in 1s"),
                "gated pods take the timeout path, got: {msg}"
            );
            assert!(
                !msg.contains("unschedulable"),
                "SchedulingGated must NOT fast-fail as unschedulable: {msg}"
            );
        }
        other => panic!("expected 503 ServiceUnavailable, got {other:?}"),
    }
    // It waited (at least the poll loop ran to the 1s deadline)…
    assert!(elapsed.as_millis() >= 900, "gated timeout took {elapsed:?}");
    // …but nowhere near the "forever" a bug would produce.
    assert!(elapsed.as_secs() < 5, "gated timeout took {elapsed:?}");
}

#[tokio::test]
async fn fast_fail_increments_the_not_ready_counter_with_reason_unschedulable() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:latest"));
    let name = sandbox_name("user-2", "chat-2", Profile::Persistent);
    store.insert_sandbox(stuck_sandbox(&name));
    store.put_pod(pending_pod(
        &name,
        "PodScheduled",
        "Unschedulable",
        "0/11 nodes are available.",
    ));

    let state = AppState::new(config(10), store.clone());
    let _ = resolve_sandbox(&state, "user-2", "chat-2", Profile::Persistent).await;

    // The frozen counter exists with its fixed reason labels (this binary's
    // recorder is seeded by AppState::new; the fast-fail fired one increment).
    let out = shared::gather();
    assert!(
        out.contains("open_websandbox_broker_sandbox_not_ready_total"),
        "missing not-ready counter:\n{out}"
    );
    for reason in ["unschedulable", "timeout", "other"] {
        assert!(
            out.contains(&format!(
                "open_websandbox_broker_sandbox_not_ready_total{{reason=\"{reason}\"}}"
            )),
            "missing reason series {reason}:\n{out}"
        );
    }
}
