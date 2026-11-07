//! B) Template-drift self-heal: every Sandbox carries a copy of the template
//! podTemplate at creation, so a template FIX never reaches existing CRs (the
//! 2026-11-06 incident: broken node-selector sandboxes stayed broken until
//! deleted by hand). The broker now stamps `owui.io/template-hash` at create
//! and recycles drifted sandboxes — but ONLY when the reaper's idle predicate
//! says nothing is using them (never parked, never within the anti-loop
//! grace, never a legacy sandbox without a stamp).

#![forbid(unsafe_code)]
// rust-stable 1.99 asserts these should be assert_eq; intentional here.
#![allow(clippy::assert_is_empty)]
use std::collections::BTreeMap;
use std::sync::Arc;

use broker::test_fakes::StubSandboxStore;
use broker::{resolve_sandbox, sandbox_name, template_hash, AppState};
use shared::{
    BrokerConfig, OperatingMode, Profile, Sandbox, SandboxSpec, SandboxTemplate,
    SandboxTemplateSpec, ShutdownPolicy,
};

const BASE_TEMPLATE: &str = "code-standard-v1";
const OLD_HASH_KEY: &str = "owui.io/template-hash";
const RECYCLED_KEY: &str = "broker-recycled-at";

fn template(image: &str) -> SandboxTemplate {
    SandboxTemplate::new(
        BASE_TEMPLATE,
        SandboxTemplateSpec {
            description: None,
            pod_template: Some(pod_json(image)),
        },
    )
}

/// The podTemplate JSON a template (and a copied-into-sandbox) blueprint is
/// made of, keyed by image — the drift lever in these tests.
fn pod_json(image: &str) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "containers": [
                {"name": "sandbox", "image": image, "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]}
            ],
            "volumes": [{"name": "workspace", "emptyDir": {}}]
        }
    })
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

/// A broker-owned persistent sandbox stamped with the OLD template's hash,
/// `broker-last-used` = `age_secs` ago, optionally Suspended/Ready.
fn drifted_sandbox(name: &str, old_hash: &str, age_secs: i64, mode: OperatingMode) -> Sandbox {
    let mut sbx = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: Some(mode),
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
    labels.insert("broker-profile".to_string(), "persistent".to_string());
    sbx.metadata.labels = Some(labels);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut annots = BTreeMap::new();
    annots.insert("broker-last-used".to_string(), (now - age_secs).to_string());
    annots.insert("broker-user".to_string(), "user-1".to_string());
    annots.insert("broker-session".to_string(), "chat-1".to_string());
    annots.insert(OLD_HASH_KEY.to_string(), old_hash.to_string());
    sbx.metadata.annotations = Some(annots);
    sbx
}

fn ready_status(ip: &str) -> shared::SandboxStatus {
    shared::SandboxStatus {
        phase: Some("Running".into()),
        pod_i_ps: Some(vec![ip.into()]),
        conditions: Some(vec![shared::SandboxCondition {
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

fn annotation(sbx: &Sandbox, key: &str) -> Option<String> {
    sbx.metadata.annotations.as_ref()?.get(key).cloned()
}

/// A LEGACY sandbox (created before `owui.io/template-hash` stamping, v0.3.0
/// and earlier): no hash annotation, but the copied `spec.podTemplate` is
/// present — that stale copy is exactly what the legacy drift comparison
/// judges.
fn legacy_sandbox(name: &str, pod_template: serde_json::Value, age_secs: i64) -> Sandbox {
    let mut sbx = drifted_sandbox(name, "unused", age_secs, OperatingMode::Running);
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .remove(OLD_HASH_KEY);
    sbx.spec.pod_template = Some(pod_template);
    sbx
}

#[tokio::test]
async fn created_sandbox_stamps_the_live_template_hash() {
    let store = Arc::new(StubSandboxStore::new());
    let tmpl = template("code-standard:latest");
    store.insert_template(tmpl.clone());
    store.set_auto_ready_on_create(Some("10.0.0.5".into()));

    let state = AppState::new(config(5), store.clone());
    resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("created + ready");

    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let snap = store.snapshot();
    let created = snap.get(&name).expect("sandbox created");
    assert_eq!(
        annotation(created, OLD_HASH_KEY).as_deref(),
        template_hash(&tmpl).as_deref(),
        "the Sandbox must carry the hash of the template it was cloned from"
    );
}

#[tokio::test]
async fn drifted_idle_sandbox_is_recycled_through_the_create_path() {
    let old = template("code-standard:BROKEN"); // e.g. the pinned node-selector
    let new = template("code-standard:FIXED");
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(new.clone()); // the live template is the fixed one
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    // Idle for an hour: nobody is using it.
    store.insert_sandbox(drifted_sandbox(
        &name,
        &template_hash(&old).unwrap(),
        3600,
        OperatingMode::Running,
    ));
    store.set_auto_ready_on_create(Some("10.0.0.7".into()));

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("recycled + ready");

    let snap = store.snapshot();
    assert_eq!(snap.len(), 1, "delete + recreate, not a second sandbox");
    let recycled = snap.get(&name).expect("sandbox recreated");
    assert_eq!(
        resolved.pod_ip, "10.0.0.7",
        "the resolve serves the FRESH sandbox"
    );
    assert_eq!(
        annotation(recycled, OLD_HASH_KEY).as_deref(),
        template_hash(&new).as_deref(),
        "recreated from the CURRENT template"
    );
    assert!(
        annotation(recycled, RECYCLED_KEY).is_some(),
        "recycle must stamp the anti-loop grace marker"
    );
    assert_eq!(
        recycled.spec.operating_mode,
        Some(OperatingMode::Running),
        "the recycled sandbox is a normal cold create"
    );
    assert_eq!(
        recycled
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("broker-profile"))
            .map(String::as_str),
        Some("persistent"),
        "the create path stamps the full broker field set"
    );
}

#[tokio::test]
async fn drifted_active_sandbox_is_left_alone() {
    let old = template("code-standard:BROKEN");
    let new = template("code-standard:FIXED");
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(new);
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut sbx = drifted_sandbox(
        &name,
        &template_hash(&old).unwrap(),
        0,
        OperatingMode::Running,
    );
    sbx.status = Some(ready_status("10.0.0.9")); // actively serving
    store.insert_sandbox(sbx);

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("active sandbox keeps serving");

    assert_eq!(resolved.pod_ip, "10.0.0.9", "the OLD sandbox still serves");
    let snap = store.snapshot();
    let kept = snap.get(&name).expect("sandbox kept");
    assert_eq!(
        annotation(kept, OLD_HASH_KEY).as_deref(),
        template_hash(&old).as_deref(),
        "an ACTIVE drifted sandbox is never recycled on resolve"
    );
    assert!(annotation(kept, RECYCLED_KEY).is_none());
}

#[tokio::test]
async fn drifted_parked_sandbox_is_never_recycled() {
    let old = template("code-standard:BROKEN");
    let new = template("code-standard:FIXED");
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(new);
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    // Suspended (parked by the reaper) and idle — still never recycled.
    store.insert_sandbox(drifted_sandbox(
        &name,
        &template_hash(&old).unwrap(),
        3600,
        OperatingMode::Suspended,
    ));

    let state = AppState::new(config(1), store.clone());
    // The parked sandbox resumes but has no status → the resolve times out;
    // the assertion is that it was NOT recycled.
    let _ = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent).await;

    let snap = store.snapshot();
    let kept = snap.get(&name).expect("parked sandbox kept");
    assert_eq!(
        kept.spec.operating_mode,
        Some(OperatingMode::Running),
        "the parked sandbox went through the normal resume path"
    );
    assert_eq!(
        annotation(kept, OLD_HASH_KEY).as_deref(),
        template_hash(&old).as_deref(),
        "a PARKED sandbox is never recycled"
    );
    assert!(annotation(kept, RECYCLED_KEY).is_none());
}

#[tokio::test]
async fn legacy_sandbox_without_hash_stamp_is_left_alone() {
    let new = template("code-standard:FIXED");
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(new);
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut sbx = drifted_sandbox(&name, "stale-but-present", 3600, OperatingMode::Running);
    // Pre-stamping CRs carry no template-hash annotation at all.
    sbx.metadata
        .annotations
        .as_mut()
        .unwrap()
        .remove(OLD_HASH_KEY);
    sbx.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(sbx);

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("legacy sandbox keeps serving");
    assert_eq!(resolved.pod_ip, "10.0.0.9");
    let snap = store.snapshot();
    let kept = snap.get(&name).expect("sandbox kept");
    assert!(
        annotation(kept, OLD_HASH_KEY).is_none(),
        "no stamp → no recycle decision"
    );
    assert!(annotation(kept, RECYCLED_KEY).is_none());
}

#[tokio::test]
async fn drifted_idle_sandbox_with_unreadable_template_keeps_serving() {
    let old = template("code-standard:BROKEN");
    let store = Arc::new(StubSandboxStore::new());
    // Live template MISSING → the hash cannot be read → keep serving.
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut sbx = drifted_sandbox(
        &name,
        &template_hash(&old).unwrap(),
        3600,
        OperatingMode::Running,
    );
    sbx.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(sbx);

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("unreadable template keeps serving");
    assert_eq!(resolved.pod_ip, "10.0.0.9");
    assert!(store.snapshot().contains_key(&name), "sandbox not deleted");
}

// --- legacy sandboxes: drift WITHOUT the owui.io/template-hash stamp --------
// (2026-11-07: pre-stamping sandboxes kept their stale copied podTemplate
// forever because drift detection keyed solely on the hash annotation. The
// controller stamps agents.x-k8s.io/sandbox-template-ref, so the template is
// knowable; without a stamp the ACTUAL spec.podTemplate is compared.)

#[tokio::test]
async fn legacy_sandbox_with_current_pod_template_is_not_recycled() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:FIXED"));
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    // Equal to the live template's podTemplate — keys built in a DIFFERENT
    // order than pod_json() to prove the comparison is canonical (key-order
    // free), not string equality.
    let pod = serde_json::json!({
        "spec": {
            "volumes": [{"name": "workspace", "emptyDir": {}}],
            "containers": [
                {"volumeMounts": [{"mountPath": "/workspace", "name": "workspace"}],
                 "image": "code-standard:FIXED", "name": "sandbox"}
            ]
        }
    });
    let mut sbx = legacy_sandbox(&name, pod, 3600);
    sbx.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(sbx);
    store.set_auto_ready_on_create(Some("10.0.0.9".into()));

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("legacy sandbox keeps serving");

    assert_eq!(resolved.pod_ip, "10.0.0.9");
    let snap = store.snapshot();
    let kept = snap.get(&name).expect("sandbox kept");
    assert!(
        annotation(kept, RECYCLED_KEY).is_none(),
        "equal ⇒ not recycled"
    );
}

#[tokio::test]
async fn legacy_sandbox_with_stale_pod_template_is_recycled_when_idle() {
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:FIXED"));
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    store.insert_sandbox(legacy_sandbox(
        &name,
        pod_json("code-standard:BROKEN"), // the copied podTemplate predates the fix
        3600,
    ));
    store.set_auto_ready_on_create(Some("10.0.0.7".into()));

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("recycled + ready");

    let snap = store.snapshot();
    assert_eq!(snap.len(), 1, "delete + recreate, not a second sandbox");
    let recycled = snap.get(&name).expect("sandbox recreated");
    assert_eq!(
        resolved.pod_ip, "10.0.0.7",
        "the resolve serves the FRESH sandbox"
    );
    assert_eq!(
        annotation(recycled, OLD_HASH_KEY).as_deref(),
        template_hash(&template("code-standard:FIXED")).as_deref(),
        "recreated from the CURRENT template — and hash-stamped going forward"
    );
    assert!(annotation(recycled, RECYCLED_KEY).is_some());
}

#[tokio::test]
async fn legacy_sandbox_with_unreadable_template_is_untouched() {
    let store = Arc::new(StubSandboxStore::new());
    // NO template in the store: the comparison basis is gone → never recycle.
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let stale = pod_json("code-standard:BROKEN");
    let mut sbx = legacy_sandbox(&name, stale.clone(), 3600);
    sbx.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(sbx);
    store.set_auto_ready_on_create(Some("10.0.0.9".into()));

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("keeps serving");

    assert_eq!(resolved.pod_ip, "10.0.0.9");
    let snap = store.snapshot();
    let kept = snap.get(&name).expect("sandbox kept");
    assert_eq!(
        kept.spec.pod_template.as_ref(),
        Some(&stale),
        "never recycle when the template cannot be read"
    );
    assert!(annotation(kept, RECYCLED_KEY).is_none());
}

#[tokio::test]
async fn legacy_sandbox_resolves_its_template_via_the_controller_ref() {
    // No owui.io/template-name (pre-stamp), but the controller stamped
    // agents.x-k8s.io/sandbox-template-ref → drift must be judged against
    // THAT template, not the base one: a sandbox matching its own custom
    // template is not drifted, even though the base template differs.
    let store = Arc::new(StubSandboxStore::new());
    store.insert_template(template("code-standard:BASE")); // base, differs
    let mut custom = template("code-standard:CUSTOM");
    custom.metadata.name = Some("code-standard-custom".into());
    store.insert_template(custom);
    let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
    let mut sbx = legacy_sandbox(&name, pod_json("code-standard:CUSTOM"), 3600);
    sbx.metadata.annotations.as_mut().unwrap().insert(
        "agents.x-k8s.io/sandbox-template-ref".to_string(),
        "code-standard-custom".to_string(),
    );
    sbx.status = Some(ready_status("10.0.0.9"));
    store.insert_sandbox(sbx);
    store.set_auto_ready_on_create(Some("10.0.0.9".into()));

    let state = AppState::new(config(5), store.clone());
    let resolved = resolve_sandbox(&state, "user-1", "chat-1", Profile::Persistent)
        .await
        .expect("keeps serving");

    assert_eq!(resolved.pod_ip, "10.0.0.9");
    let snap = store.snapshot();
    let kept = snap.get(&name).expect("sandbox kept");
    assert!(
        annotation(kept, RECYCLED_KEY).is_none(),
        "matches its OWN template"
    );
}
