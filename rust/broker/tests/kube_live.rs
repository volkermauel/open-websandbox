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
//! **Omitted (out of budget / not reliably testable in-process):**
//!   * the full leader-election RACE (two brokers racing one lease) — `KubeLease`
//!     derives its identity from `HOSTNAME`/pid, which two in-process instances
//!     share, so a fair race cannot be constructed; the deterministic
//!     expired-holder takeover covers the takeover branch instead;
//!   * the WS terminal relay (`terminal.rs`) — needs a deployed runtime pod with
//!     a PTY-equivalent upstream, heavier than this pass wires up.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use broker::reaper::MANAGED_BY_SELECTOR;
use broker::sandbox::build_sandbox;
use broker::{build_client, KubeLease, KubeSandboxStore, LeaseClient, SandboxStore};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff;
use kube::api::{Api, ObjectMeta, PostParams};
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
/// in the past that it is expired, is taken over by `acquire_or_renew`. (The
/// full two-broker race is omitted — see the module docs.)
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
