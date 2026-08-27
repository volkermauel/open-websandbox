//! Live Kubernetes integration tests (issue #101, item **C1**).
//!
//! Exercises the REAL kube-rs backends against a live KIND cluster:
//!   * [`KubeSandboxStore`] — create / list / get / delete a `Sandbox`
//!     (`agents.x-k8s.io/v1beta1`) + the per-session runtime-key `Secret`
//!     (`ensure`/`read`/`delete`) round-trip;
//!   * [`KubeLease`] — acquire / renew / release a `coordination.k8s.io/v1`
//!     `Lease`, plus a deterministic expired-holder takeover (the unit-testable
//!     takeover branch driven against the real apiserver).
//!
//! **Env-gated**: returns (passes) unless `OWUI_KUBE_LIVE=1`. Requires the
//! upstream CRDs applied and `KUBECONFIG` pointed at the test cluster. The client
//! is built with [`broker::build_client`] (in-cluster SA → local kubeconfig), so
//! the test has no cluster-specific wiring:
//!
//! ```text
//! # R1: the KIND cluster gets its OWN kubeconfig path — never ~/.kube/config.
//! kind create cluster --name owui-c1c2 --kubeconfig /tmp/owui-c1c2.kubeconfig
//! KUBECONFIG=/tmp/owui-c1c2.kubeconfig \
//!   kubectl apply -f open-websandbox-platform/upstream/sandbox-with-extensions-v0.5.3.yaml
//! KUBECONFIG=/tmp/owui-c1c2.kubeconfig OWUI_KUBE_LIVE=1 \
//!   cargo test -p broker --test kube_live -- --nocapture
//! ```
//!
//! Every test uses a unique object name (pid + monotonic seq) inside one shared
//! namespace, so parallel test threads never collide.
//!
//! **Leader election:** acquire/renew/release, a deterministic expired-holder
//!   takeover, AND the full two-broker RACE for one lease. `KubeLease` derives its
//!   holder identity from `HOSTNAME`/pid — two in-process instances would share it
//!   and never compete — so the race uses the test-only `KubeLease::with_identity`
//!   seam to give each competitor a distinct holder (see
//!   `lease_two_brokers_race_for_one_lease`).
//!
//! **Out of scope here:** the WS terminal relay (`terminal.rs`) — it needs a
//!   deployed runtime pod; covered instead in `tests/ws_relay.rs` against a local
//!   echo server.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use broker::reaper::MANAGED_BY_SELECTOR;
use broker::sandbox::build_sandbox;
use broker::store::WorkspacePvcSpec;
use broker::{build_client, KubeLease, KubeSandboxStore, LeaseClient, SandboxStore, StoreError};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff;
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams};
use kube::ResourceExt;
use shared::{BrokerConfig, Profile};

/// Run only when the operator opted in (`OWUI_KUBE_LIVE=1`); otherwise every
/// test returns (passes) so a plain `cargo test` needs no cluster.
fn gated() -> bool {
    std::env::var("OWUI_KUBE_LIVE").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// Shared test namespace (created idempotently). Every test uses unique object
/// names within it, so parallel test threads never collide.
const NS: &str = "owui-kube-live";

/// Monotonic per-process counter so every test uses a unique object name.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique suffix: `<pid>-<seq>-<tag>`.
fn uniq(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("owui-live-{n}-{}-{}", tag, std::process::id())
}

/// Build a kube client pointed at the cluster in `$KUBECONFIG` via the shared
/// in-cluster→kubeconfig inference the binary itself uses.
/// Install the aws-lc-rs rustls CryptoProvider once per process. The broker
/// binary does this at boot (`main.rs`); the test harness must too, or the
/// kube/reqwest rustls client panics with "no process-level CryptoProvider".
/// `install_default` is idempotent (returns `Err` if already set), so the
/// `OnceLock` makes the first call win and later ones no-op.
fn install_crypto() {
    static CRYPTO: OnceLock<()> = OnceLock::new();
    CRYPTO.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Build a kube client pointed at the cluster in `$KUBECONFIG` via the shared
/// in-cluster→kubeconfig inference the binary itself uses.
async fn client() -> kube::Client {
    install_crypto();
    build_client()
        .await
        .expect("kube client (set KUBECONFIG to the test cluster and apply the CRDs)")
}

/// Idempotently create the shared test namespace (tolerates a parallel 409).
async fn ensure_ns(client: &kube::Client) {
    use k8s_openapi::api::core::v1::Namespace;
    let api: Api<Namespace> = Api::all(client.clone());
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(NS.to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    match api.create(&PostParams::default(), &ns).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {} // already exists
        Err(e) => panic!("create namespace {NS}: {e}"),
    }
}

/// A minimal-but-real pod blueprint (emptyDir workspace) `build_sandbox` clones
/// into the `Sandbox` (the strict `podTemplate` CRD schema validates this).
fn base_pod_template() -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "containers": [{
                "name": "sandbox",
                "image": "code-standard:latest",
                "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]
            }],
            "volumes": [{"name": "workspace", "emptyDir": {}}]
        }
    })
}

#[tokio::test]
async fn sandbox_crud_create_get_list_delete() {
    if !gated() {
        eprintln!(
            "skipped: set OWUI_KUBE_LIVE=1 (and point KUBECONFIG at a test cluster with the CRDs applied)"
        );
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let store = KubeSandboxStore::new(client, NS);
    let name = uniq("c");

    let sbx = build_sandbox(
        &name,
        Some("user-live"),
        Some("chat-live"),
        Profile::Persistent,
        base_pod_template(),
        NS,
        1_700_000_000,
    );

    // Create.
    let created = store.create_sandbox(sbx).await.expect("create_sandbox");
    assert_eq!(created.name_any(), name);

    // Get.
    let got = store
        .get_sandbox(&name)
        .await
        .expect("get_sandbox")
        .expect("sandbox present after create");
    assert_eq!(got.name_any(), name);
    assert_eq!(
        got.metadata.labels.as_ref().unwrap()["app.kubernetes.io/managed-by"],
        "owui-broker",
        "the broker-managed-by label round-trips through the apiserver"
    );

    // List filtered by the managed-by selector includes ours.
    let listed = store
        .list_sandboxes(Some(MANAGED_BY_SELECTOR))
        .await
        .expect("list_sandboxes");
    assert!(
        listed.iter().any(|s| s.name_any() == name),
        "our sandbox appears in the managed-by list"
    );

    // Delete → true, then again → false (404-tolerant), then get → None.
    assert!(
        store.delete_sandbox(&name).await.expect("delete #1"),
        "first delete returns true (existed)"
    );
    assert!(
        !store.delete_sandbox(&name).await.expect("delete #2"),
        "second delete returns false (already gone)"
    );
    assert!(
        store
            .get_sandbox(&name)
            .await
            .expect("get after delete")
            .is_none(),
        "sandbox is gone after delete"
    );
}

#[tokio::test]
async fn runtime_key_secret_roundtrip() {
    if !gated() {
        eprintln!("skipped: set OWUI_KUBE_LIVE=1 to run the live K8s tests");
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let store = KubeSandboxStore::new(client, NS);
    let name = uniq("key");

    // read before ensure ⇒ None (no Secret yet).
    assert!(
        store
            .read_runtime_key(&name)
            .await
            .expect("read #1")
            .is_none(),
        "no key before ensure_runtime_key"
    );

    // ensure creates the per-session Secret.
    store.ensure_runtime_key(&name).await.expect("ensure #1");
    let k1 = store
        .read_runtime_key(&name)
        .await
        .expect("read #2")
        .expect("key present after ensure");
    assert!(!k1.is_empty(), "minted key is non-empty");

    // ensure is idempotent — it does NOT rotate an existing key.
    store.ensure_runtime_key(&name).await.expect("ensure #2");
    let k2 = store
        .read_runtime_key(&name)
        .await
        .expect("read #3")
        .expect("key still present");
    assert_eq!(k1, k2, "ensure_runtime_key never rotates an existing key");

    // delete → read None.
    store.delete_runtime_key(&name).await.expect("delete");
    assert!(
        store
            .read_runtime_key(&name)
            .await
            .expect("read #4")
            .is_none(),
        "key gone after delete"
    );
}

/// Acquire (create) → renew → release (delete) → re-acquire the leader lease.
#[tokio::test]
async fn lease_acquire_renew_release_cycle() {
    if !gated() {
        eprintln!("skipped: set OWUI_KUBE_LIVE=1 to run the live K8s tests");
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let lease_name = uniq("lease");
    let cfg = BrokerConfig {
        leader_namespace: NS.to_string(),
        leader_lease: lease_name.clone(),
        leader_duration_seconds: 15,
        ..Default::default()
    };
    let lease = KubeLease::new(client.clone(), &cfg);

    // Fresh acquire (creates the Lease with us as holder).
    assert!(lease.acquire_or_renew().await, "fresh acquire");
    // Renew (we hold it) — still true.
    assert!(lease.acquire_or_renew().await, "renew while holding");
    // Release deletes it.
    lease.release().await;
    assert!(
        lease_is_absent(&client, NS, &lease_name).await,
        "release deleted the lease"
    );
    // Re-acquire works immediately after release.
    assert!(lease.acquire_or_renew().await, "re-acquire after release");
    lease.release().await;
}

/// Deterministic takeover: a lease held by ANOTHER identity, renewed far enough
/// in the past that it is expired, is taken over by `acquire_or_renew`. The full
/// two-broker race lives in `lease_two_brokers_race_for_one_lease`.
#[tokio::test]
async fn lease_takeover_when_holder_expired() {
    if !gated() {
        eprintln!("skipped: set OWUI_KUBE_LIVE=1 to run the live K8s tests");
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let lease_name = uniq("lease-tk");

    // Seed a Lease held by another broker, renewed 1 hour ago with a 15s
    // duration ⇒ well past expiry, so our acquire must take it over.
    seed_expired_lease(&client, NS, &lease_name, "other-broker", 3600, 15).await;

    let cfg = BrokerConfig {
        leader_namespace: NS.to_string(),
        leader_lease: lease_name.clone(),
        leader_duration_seconds: 15,
        ..Default::default()
    };
    let lease = KubeLease::new(client.clone(), &cfg);
    assert!(
        lease.acquire_or_renew().await,
        "acquire takes over an expired foreign lease"
    );
    // We now hold it (holder changed away from the seeded identity).
    let holder = lease_holder(&client, NS, &lease_name).await;
    assert_ne!(
        holder.as_deref(),
        Some("other-broker"),
        "holder flipped to us after takeover"
    );
    lease.release().await;
}

/// Real two-broker leader-election race for ONE lease via distinct injected
/// identities (`KubeLease::with_identity`). Deterministic — no pure timing race:
/// broker-A acquires first; broker-B cannot steal while A holds and renews; once
/// A's renewal is aged past the duration, B takes over and A then defers. This is
/// the real race the takeover test above stands in for.
#[tokio::test]
async fn lease_two_brokers_race_for_one_lease() {
    if !gated() {
        eprintln!("skipped: set OWUI_KUBE_LIVE=1 to run the live K8s tests");
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let lease_name = uniq("race");
    let cfg = BrokerConfig {
        leader_namespace: NS.to_string(),
        leader_lease: lease_name.clone(),
        leader_duration_seconds: 15,
        ..Default::default()
    };
    let broker_a = KubeLease::new(client.clone(), &cfg).with_identity("broker-A");
    let broker_b = KubeLease::new(client.clone(), &cfg).with_identity("broker-B");

    // 1. A acquires the empty lease first.
    assert!(broker_a.acquire_or_renew().await, "broker-A acquires first");
    assert_eq!(
        lease_holder(&client, NS, &lease_name).await.as_deref(),
        Some("broker-A"),
        "holder is broker-A"
    );

    // 2. B cannot steal while A holds a LIVE lease.
    assert!(
        !broker_b.acquire_or_renew().await,
        "broker-B defers while broker-A holds a live lease"
    );
    assert_eq!(
        lease_holder(&client, NS, &lease_name).await.as_deref(),
        Some("broker-A"),
        "holder unchanged after broker-B's failed steal"
    );

    // 3. A renews (keeps the lease); B retries and still defers.
    assert!(broker_a.acquire_or_renew().await, "broker-A renews");
    assert!(
        !broker_b.acquire_or_renew().await,
        "broker-B still defers right after broker-A renewed"
    );
    assert_eq!(
        lease_holder(&client, NS, &lease_name).await.as_deref(),
        Some("broker-A"),
        "holder still broker-A after renew + retry"
    );

    // 4. Simulate A's renewal stalling past the duration (deterministic, no
    //    real-time wait): age the lease out so A's claim reads as expired.
    age_out_lease(&client, NS, &lease_name, "broker-A", 3600, 15).await;

    // 5. B now takes over the expired lease.
    assert!(
        broker_b.acquire_or_renew().await,
        "broker-B takes over once broker-A's renewal is past the duration"
    );
    assert_eq!(
        lease_holder(&client, NS, &lease_name).await.as_deref(),
        Some("broker-B"),
        "holder flipped to broker-B after takeover"
    );

    // 6. A now defers to B.
    assert!(
        !broker_a.acquire_or_renew().await,
        "broker-A defers to broker-B after losing the lease"
    );
    assert_eq!(
        lease_holder(&client, NS, &lease_name).await.as_deref(),
        Some("broker-B"),
        "holder remains broker-B"
    );

    broker_b.release().await;
}

/// Server-side rewrite of an existing Lease's `renewTime`/`acquireTime` to
/// `age_secs` in the past (keeping `holder`), so the next `acquire_or_renew`
/// from a DIFFERENT identity deterministically takes it over — mirroring how a
/// real holder looks once its renewal cadence stalls past `duration`. Unlike
/// `seed_expired_lease` (which `create`s), this `replace`s an existing lease.
async fn age_out_lease(
    client: &kube::Client,
    ns: &str,
    name: &str,
    holder: &str,
    age_secs: i64,
    duration: i32,
) {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    let mut existing = api
        .get(name)
        .await
        .unwrap_or_else(|e| panic!("get lease {name} to age out: {e}"));
    let now = jiff::Timestamp::now();
    let past = jiff::Timestamp::from_second(now.as_second().saturating_sub(age_secs))
        .expect("valid past epoch seconds");
    let mut spec = existing.spec.clone().unwrap_or_default();
    spec.holder_identity = Some(holder.to_string());
    spec.lease_duration_seconds = Some(duration);
    spec.acquire_time = Some(MicroTime::from(past));
    spec.renew_time = Some(MicroTime::from(past));
    existing.spec = Some(spec);
    api.replace(name, &PostParams::default(), &existing)
        .await
        .unwrap_or_else(|e| panic!("age out lease {name}: {e}"));
}
/// Seed a `coordination.k8s.io/v1` Lease held by `holder`, with `renewTime`
/// `age_secs` in the past and `leaseDurationSeconds = duration`, so a later
/// `acquire_or_renew` deterministically takes it over.
async fn seed_expired_lease(
    client: &kube::Client,
    ns: &str,
    name: &str,
    holder: &str,
    age_secs: i64,
    duration: i32,
) {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    let now = jiff::Timestamp::now();
    let past = jiff::Timestamp::from_second(now.as_second().saturating_sub(age_secs))
        .expect("valid past epoch seconds");
    let lease = Lease {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(holder.to_string()),
            lease_duration_seconds: Some(duration),
            acquire_time: Some(MicroTime::from(past)),
            renew_time: Some(MicroTime::from(past)),
            ..Default::default()
        }),
    };
    api.create(&PostParams::default(), &lease)
        .await
        .unwrap_or_else(|e| panic!("seed expired lease {name}: {e}"));
}

/// Current `holderIdentity` of the named Lease, or `None` when absent.
async fn lease_holder(client: &kube::Client, ns: &str, name: &str) -> Option<String> {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    api.get(name)
        .await
        .ok()
        .and_then(|l| l.spec)
        .and_then(|s| s.holder_identity)
}

/// `true` when the named Lease no longer exists (HTTP 404).
async fn lease_is_absent(client: &kube::Client, ns: &str, name: &str) -> bool {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    matches!(api.get(name).await, Err(kube::Error::Api(e)) if e.code == 404)
}

// --- ensure_workspace_pvc (#140): idempotency + concurrent-create race ------
// The per-user PVC is created lazily on first resolve; two chats of the same
// user resolving concurrently (or a retry after a timeout) both call
// `ensure_workspace_pvc` with the same deterministic name. The real-API
// contract: every caller succeeds (a 409 from the loser is swallowed) and
// exactly one PVC object exists. The shared-subpath mode (`create: None`) is
// an existence check that must surface a missing chart PVC as NotFound.

/// The spec the broker derives from its config for a per-user workspace PVC
/// (cluster-default storage class → KIND local-path; the PVC is never bound
/// here — `ensure` only creates the object, binding waits for a consumer).
fn workspace_pvc_spec() -> WorkspacePvcSpec {
    WorkspacePvcSpec {
        access_modes: vec!["ReadWriteOnce".to_string()],
        storage: "1Gi".to_string(),
        storage_class: String::new(),
    }
}

/// Delete the named PVC (404-tolerant) so repeated live runs stay tidy.
async fn delete_pvc(client: &kube::Client, name: &str) {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NS);
    match pvcs.delete(name, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => panic!("delete pvc {name}: {e}"),
    }
}

/// How many PVCs named `name` exist in the shared test namespace.
async fn count_pvcs(client: &kube::Client, name: &str) -> usize {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NS);
    pvcs.list(&ListParams::default().fields(&format!("metadata.name={name}")))
        .await
        .unwrap_or_else(|e| panic!("list pvcs {name}: {e}"))
        .items
        .len()
}

#[tokio::test]
async fn workspace_pvc_ensure_is_idempotent_and_carries_spec() {
    if !gated() {
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let store = KubeSandboxStore::new(client.clone(), NS);
    let name = uniq("pvc");
    let spec = workspace_pvc_spec();

    store
        .ensure_workspace_pvc(&name, Some(&spec))
        .await
        .expect("first ensure creates the PVC");
    // A second resolve of the same user hits the existing object: the 409 that
    // a naive create would return must be swallowed (idempotent success).
    store
        .ensure_workspace_pvc(&name, Some(&spec))
        .await
        .expect("second ensure tolerates AlreadyExists");
    // The shared-mode existence check on a PRESENT PVC also succeeds.
    store
        .ensure_workspace_pvc(&name, None)
        .await
        .expect("existence check succeeds for a present PVC");

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NS);
    let got = pvcs.get(&name).await.expect("pvc readable");
    assert_eq!(count_pvcs(&client, &name).await, 1, "exactly one PVC");
    let s = got.spec.expect("pvc spec");
    assert_eq!(
        s.access_modes.as_deref(),
        Some(["ReadWriteOnce".to_string()].as_slice()),
        "access modes from the spec"
    );
    assert_eq!(
        s.resources
            .and_then(|r| r.requests)
            .and_then(|m| m.get("storage").cloned())
            .map(|q| q.0),
        Some("1Gi".to_string()),
        "storage request from the spec"
    );
    delete_pvc(&client, &name).await;
}

#[tokio::test]
async fn workspace_pvc_concurrent_ensure_creates_exactly_one() {
    if !gated() {
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let name = uniq("pvc-race");
    let spec = workspace_pvc_spec();

    // Eight concurrent resolves of the same user (two chats × four retries, or
    // a burst from Open Web UI) race the create. JoinSet drives them in
    // parallel on the shared test runtime, exactly like concurrent requests.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = KubeSandboxStore::new(client.clone(), NS);
        let name = name.clone();
        let spec = spec.clone();
        set.spawn(async move { store.ensure_workspace_pvc(&name, Some(&spec)).await });
    }
    while let Some(joined) = set.join_next().await {
        joined
            .expect("task panicked")
            .unwrap_or_else(|e| panic!("concurrent ensure failed: {e}"));
    }
    assert_eq!(
        count_pvcs(&client, &name).await,
        1,
        "the race must converge on exactly one PVC"
    );
    delete_pvc(&client, &name).await;
}

#[tokio::test]
async fn workspace_pvc_shared_mode_missing_pvc_is_not_found() {
    if !gated() {
        return;
    }
    let client = client().await;
    ensure_ns(&client).await;
    let store = KubeSandboxStore::new(client.clone(), NS);
    let name = uniq("pvc-missing");

    // shared-subpath: the chart owns the PVC. A missing one is an install
    // misconfiguration and must surface as NotFound, not be papered over by a
    // silent create.
    match store.ensure_workspace_pvc(&name, None).await {
        Err(StoreError::NotFound) => {}
        other => panic!("expected NotFound for a missing shared PVC, got {other:?}"),
    }
}
