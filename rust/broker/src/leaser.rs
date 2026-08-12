//! Leader election over a `coordination.k8s.io/v1` `Lease`.
//!
//! Single-lease election (D11): exactly one
//! broker holds a named `Lease` and is allowed to run the idle reaper, so two
//! replicas never double-park/double-reap the same sandbox. The request path
//! (proxy/resolve) runs on every replica regardless of leadership — only the
//! reaper is leader-gated (see [`crate::reaper`]).
//!
//! ## Parameters (`_LEADER_*`)
//!
//! * namespace — `BROKER_LEADER_NAMESPACE` (defaults to the runtime namespace).
//! * name — `BROKER_LEADER_LEASE` (default `owui-broker-leader`).
//! * identity — `HOSTNAME` env, else `broker-<pid>`.
//! * duration — `BROKER_LEADER_DURATION_SECONDS` (default 15); a holder whose
//!   `renewTime` is older than this is expired and another broker takes over.
//! * renew cadence — `BROKER_LEADER_RENEW_SECONDS` (default 5); the loop sleeps
//!   this between acquire/renew attempts (well under `duration`).
//!
//! ## Testability
//!
//! The lease backend is behind the [`LeaseClient`] trait so the gate logic is
//! exercised against [`InMemoryLease`] with an injected clock — no apiserver
//! required. [`KubeLease`] is the production backend over `kube::Api<Lease>`.

#![forbid(unsafe_code)]

use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff;
use kube::api::{Api, DeleteParams, PostParams};
use tokio::sync::watch;

/// A shared, atomically-updated "am I the elected leader" flag the leader loop
/// sets and the reaper / request path reads. Cheap to clone; every clone sees
/// the same value.
#[derive(Debug, Clone)]
pub struct LeaderGate(Arc<AtomicBool>);

impl LeaderGate {
    /// New gate, initially `false` (no leader until the loop acquires the lease).
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// True iff this broker currently holds the leader lease (and may reap).
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Leader-loop-only setter (the loop is the sole writer).
    pub(crate) fn set(&self, leading: bool) {
        self.0.store(leading, Ordering::SeqCst);
    }
}

impl Default for LeaderGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The lease backend: acquire/renew/release the leader lease. Behind a trait so
/// the gate + loop logic is unit-testable against an in-memory double.
#[async_trait]
pub trait LeaseClient: Send + Sync {
    /// Acquire the lease if absent, renew it if we hold it, take it over if the
    /// current holder's claim has expired, or defer (return `false`) if another
    /// live holder owns it.
    async fn acquire_or_renew(&self) -> bool;

    /// Best-effort release of the lease on graceful shutdown so a fast restart
    /// re-acquires without waiting for `duration` to elapse. Only meaningful when
    /// we currently hold it; 404 (already gone) is not an error.
    async fn release(&self);
}

/// Resolve the broker identity: `HOSTNAME` (set on every pod) or `broker-<pid>`.
fn resolve_identity() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| format!("broker-{}", process::id()))
}

/// Production lease backend over `kube::Api<Lease>`
/// (`coordination.k8s.io/v1`).
#[derive(Clone)]
pub struct KubeLease {
    client: kube::Client,
    namespace: String,
    name: String,
    identity: String,
    /// `leaseDurationSeconds` (int32 on the wire; clamped ≥ 1).
    duration_seconds: i32,
}

impl KubeLease {
    /// Build the lease backend from the kube client + broker config.
    #[must_use]
    pub fn new(client: kube::Client, cfg: &shared::BrokerConfig) -> Self {
        Self {
            client,
            namespace: cfg.leader_namespace.clone(),
            name: cfg.leader_lease.clone(),
            identity: resolve_identity(),
            duration_seconds: cfg.leader_duration_seconds.max(1).min(i32::MAX as u64) as i32,
        }
    }
}

/// True iff the kube error is an HTTP 404 (the lease doesn't exist yet / anymore).
fn is_not_found(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(e) if e.code == 404)
}

#[async_trait]
impl LeaseClient for KubeLease {
    async fn acquire_or_renew(&self) -> bool {
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.get(&self.name).await {
            // Lease exists: renew if ours / unclaimed, take over if expired, else defer.
            Ok(existing) => {
                let spec = existing.spec.clone().unwrap_or_default();
                let duration = spec.lease_duration_seconds.unwrap_or(self.duration_seconds) as i64;
                let held_by_other_live =
                    match (spec.holder_identity.as_deref(), spec.renew_time.as_ref()) {
                        (Some(holder), Some(rt)) if holder != self.identity => {
                            // age = now - renewTime (seconds); expired once age >= duration.
                            let age = jiff::Timestamp::now()
                                .as_second()
                                .saturating_sub(rt.0.as_second());
                            age < duration
                        }
                        _ => false,
                    };
                if held_by_other_live {
                    return false;
                }
                let now = jiff::Timestamp::now();
                let mut next_spec = spec;
                next_spec.holder_identity = Some(self.identity.clone());
                next_spec.lease_duration_seconds = Some(self.duration_seconds);
                next_spec.renew_time = Some(MicroTime::from(now));
                if next_spec.acquire_time.is_none() {
                    next_spec.acquire_time = Some(MicroTime::from(now));
                }
                let mut updated = existing;
                updated.spec = Some(next_spec);
                match api
                    .replace(&self.name, &PostParams::default(), &updated)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "leader: renew lease failed");
                        false
                    }
                }
            }
            // No lease yet: create it with ourselves as the holder.
            Err(e) if is_not_found(&e) => {
                let now = jiff::Timestamp::now();
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(self.name.clone()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(self.identity.clone()),
                        lease_duration_seconds: Some(self.duration_seconds),
                        acquire_time: Some(MicroTime::from(now)),
                        renew_time: Some(MicroTime::from(now)),
                        ..Default::default()
                    }),
                };
                match api.create(&PostParams::default(), &lease).await {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "leader: create lease failed");
                        false
                    }
                }
            }
            // Transient read failure: defer (treat as not-leader until next tick).
            Err(e) => {
                tracing::warn!(error = %e, "leader: read lease failed");
                false
            }
        }
    }

    async fn release(&self) {
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Err(e) = api.delete(&self.name, &DeleteParams::default()).await {
            if !is_not_found(&e) {
                tracing::warn!(error = %e, "leader: release lease failed");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory lease double (tests): shared (holder, renew_secs, duration) state
// with an injectable clock so expiry/take-over is deterministic, no apiserver.
// ---------------------------------------------------------------------------

#[cfg(test)]
/// A clock the in-memory lease reads "now" from (epoch seconds); tests inject
/// a controllable one so expiry/take-over is deterministic.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Shared in-memory lease state: `(holder, renew_time_secs, duration_secs)`.
/// `None` ⇒ no lease created yet. Clones share the same cell.
#[derive(Debug, Default, Clone)]
pub struct InMemoryLease(Arc<Mutex<Option<(String, i64, i64)>>>);

impl InMemoryLease {
    /// New empty lease (no holder).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current holder identity, if any (test introspection).
    #[must_use]
    pub fn holder(&self) -> Option<String> {
        self.0
            .lock()
            .expect("lease lock")
            .as_ref()
            .map(|(h, _, _)| h.clone())
    }
}

/// A [`LeaseClient`] over a shared [`InMemoryLease`] with a fixed identity and
/// an injectable clock — the unit-test backend for the leader gate.
#[cfg(test)]
#[derive(Clone)]
pub struct InMemoryLeaseClient {
    state: InMemoryLease,
    identity: String,
    duration_seconds: i64,
    now: Clock,
}

#[cfg(test)]
impl InMemoryLeaseClient {
    /// Test client: `identity` is this broker, `duration_seconds` its lease TTL,
    /// `now` the injected clock (epoch seconds).
    #[must_use]
    pub fn new(identity: &str, duration_seconds: i64, now: Clock) -> Self {
        Self {
            state: InMemoryLease::new(),
            identity: identity.to_string(),
            duration_seconds,
            now,
        }
    }

    /// Wrap an existing shared lease (so two brokers race for ONE cell).
    #[must_use]
    pub fn for_state(&self, identity: &str) -> Self {
        Self {
            state: self.state.clone(),
            identity: identity.to_string(),
            duration_seconds: self.duration_seconds,
            now: self.now.clone(),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl LeaseClient for InMemoryLeaseClient {
    async fn acquire_or_renew(&self) -> bool {
        let now = (self.now)();
        let mut guard = self.state.0.lock().expect("lease lock");
        match &*guard {
            None => {
                // Create.
                *guard = Some((self.identity.clone(), now, self.duration_seconds));
                true
            }
            Some((holder, renew, duration)) if holder != &self.identity => {
                // Held by another: take over only if expired.
                let age = now.saturating_sub(*renew);
                if age >= *duration {
                    *guard = Some((self.identity.clone(), now, self.duration_seconds));
                    true
                } else {
                    false
                }
            }
            Some(_) => {
                // Ours: renew (bump renew_time).
                *guard = Some((self.identity.clone(), now, self.duration_seconds));
                true
            }
        }
    }

    async fn release(&self) {
        let mut guard = self.state.0.lock().expect("lease lock");
        if let Some((holder, _, _)) = &*guard {
            if holder == &self.identity {
                *guard = None;
            }
        }
    }
}

/// The leader loop: periodically acquire/renew the lease, reflect ownership into
/// [`LeaderGate`], and on shutdown step down (release the lease + clear the gate).
///
/// The reaper loop is a *separate* always-alive task that no-ops while the gate
/// is `false` (see [`crate::reaper::run_reaper_loop`]); this loop only owns the
/// lease + the gate, which keeps the leadership state machine small and testable.
pub async fn run_leader_loop(
    lease: Arc<dyn LeaseClient>,
    gate: Arc<LeaderGate>,
    renew_interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let leading = lease.acquire_or_renew().await;
        let was = gate.is_leader();
        gate.set(leading);
        if leading && !was {
            tracing::info!("acquired leader lease — this broker may reap");
        } else if !leading && was {
            tracing::info!("lost leader lease — reaper will skip");
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    tracing::debug!("leader loop: shutdown sender dropped");
                }
                break;
            }
            _ = tokio::time::sleep(renew_interval) => {}
        }
    }
    tracing::info!("leader loop shutting down — stepping down");
    if gate.is_leader() {
        lease.release().await;
        gate.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;

    fn ticking_clock(start: i64) -> (Clock, Arc<AtomicI64>) {
        let cell = Arc::new(AtomicI64::new(start));
        let c = cell.clone();
        let clock: Clock = Arc::new(move || c.load(Ordering::SeqCst));
        (clock, cell)
    }

    #[tokio::test]
    async fn first_broker_acquires_then_second_defers() {
        let (clock, _now) = ticking_clock(1000);
        let a = InMemoryLeaseClient::new("broker-a", 15, clock.clone());
        let b = a.for_state("broker-b");
        assert!(
            a.acquire_or_renew().await,
            "broker-a acquires the empty lease"
        );
        assert!(
            !b.acquire_or_renew().await,
            "broker-b defers while broker-a holds a live lease"
        );
        assert_eq!(a.state.holder().as_deref(), Some("broker-a"));
    }

    #[tokio::test]
    async fn holder_renews_and_stays_leader() {
        let (clock, now) = ticking_clock(1000);
        let a = InMemoryLeaseClient::new("broker-a", 15, clock.clone());
        assert!(a.acquire_or_renew().await);
        now.store(1005, Ordering::SeqCst); // within duration
        assert!(a.acquire_or_renew().await, "renew keeps broker-a as holder");
        assert_eq!(a.state.holder().as_deref(), Some("broker-a"));
    }

    #[tokio::test]
    async fn expired_holder_is_taken_over() {
        let (clock, now) = ticking_clock(1000);
        let a = InMemoryLeaseClient::new("broker-a", 15, clock.clone());
        let b = a.for_state("broker-b");
        assert!(a.acquire_or_renew().await);
        now.store(1020, Ordering::SeqCst); // 20s later — past the 15s duration
        assert!(
            b.acquire_or_renew().await,
            "broker-b takes over once broker-a's renew is older than duration"
        );
        assert!(
            !a.acquire_or_renew().await,
            "broker-a now defers to broker-b"
        );
        assert_eq!(a.state.holder().as_deref(), Some("broker-b"));
    }

    #[tokio::test]
    async fn release_clears_so_another_can_acquire_immediately() {
        let (clock, _now) = ticking_clock(1000);
        let a = InMemoryLeaseClient::new("broker-a", 15, clock.clone());
        let b = a.for_state("broker-b");
        assert!(a.acquire_or_renew().await);
        a.release().await;
        assert!(a.state.holder().is_none(), "release clears the lease cell");
        assert!(
            b.acquire_or_renew().await,
            "after release broker-b acquires without waiting for expiry"
        );
    }

    #[tokio::test]
    async fn release_only_drops_when_we_hold_it() {
        let (clock, _now) = ticking_clock(1000);
        let a = InMemoryLeaseClient::new("broker-a", 15, clock.clone());
        let b = a.for_state("broker-b");
        assert!(a.acquire_or_renew().await);
        // broker-b never held it — its release must be a no-op.
        b.release().await;
        assert_eq!(a.state.holder().as_deref(), Some("broker-a"));
    }

    #[tokio::test]
    async fn leader_loop_sets_gate_and_steps_down_on_shutdown() {
        let (clock, _now) = ticking_clock(1000);
        let client = Arc::new(InMemoryLeaseClient::new("broker-a", 15, clock.clone()));
        let gate = Arc::new(LeaderGate::new());
        let (tx, rx) = watch::channel(false);
        let lease: Arc<dyn LeaseClient> = client.clone();
        let g = gate.clone();
        let handle =
            tokio::spawn(
                async move { run_leader_loop(lease, g, Duration::from_millis(10), rx).await },
            );

        // Let one acquire tick land.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(gate.is_leader(), "gate reflects the acquired lease");
        assert_eq!(client.state.holder().as_deref(), Some("broker-a"));

        // Trigger graceful shutdown.
        tx.send(true).expect("send shutdown");
        // The loop steps down: releases the lease + clears the gate.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handle.is_finished(), "leader loop exited on shutdown");
        assert!(!gate.is_leader(), "gate cleared on step-down");
        assert!(
            client.state.holder().is_none(),
            "lease released on graceful shutdown"
        );
    }
}
