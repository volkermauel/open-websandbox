//! Leader-gated idle reaper — parks + reaps broker-owned `Sandbox` objects.
//!
//! Periodically lists every `Sandbox` labelled
//! `app.kubernetes.io/managed-by=owui-broker`, reads each one's idle time from
//! its `broker-last-used` annotation (`now - last_used`), and for those past
//! their threshold:
//!
//! * **ephemeral** (emptyDir) → **reap** (delete) once idle past `idle_ttl_seconds`.
//! * **persistent** (PVC-backed) → **park** (`spec.operatingMode: Suspended`) once
//!   idle past `park_idle_seconds`; **reap** (delete) once idle past
//!   `reap_seconds` (7 days). Reap takes precedence over park, and an
//!   already-`Suspended` sandbox is never re-parked.
//!
//! The leader election ([`crate::leaser`]) gates the *whole* loop: only the elected
//! broker reaps, so two replicas never double-park/double-reap the same sandbox.
//! Non-leaders still serve the read/proxy path.
//!
//! ## The C-4 offload seam
//!
//! Before deleting a sandbox the reaper hands it to [`ReapOffload::offload_on_reap`].
//! C-3 ships a [`NoopOffload`] that always succeeds (delete proceeds immediately);
//! C-4 replaces it with the S3-tiered offload, which streams `/workspace → S3`
//! and — on failure — returns `Err` so the reaper **keeps the sandbox alive for
//! the next tick** (D7: never silently lose a snapshot). The C-4 impl no-ops for
//! non-s3-tiered sandboxes — it only offloads `persistent` + `s3-tiered`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kube::ResourceExt;
use shared::{OperatingMode, Profile, Sandbox};

use crate::leaser::LeaderGate;
use crate::metrics::{ACTIVE_SANDBOXES, IDLE_REAPS_TOTAL, SANDBOXES_DELETED_TOTAL};
use crate::s3::S3_LAST_KEY_ANNOTATION;
use crate::sandbox::{LAST_USED_KEY, PROFILE_LABEL_KEY};
use crate::store::SandboxStore;

/// Label selector matching exactly the broker-owned `Sandbox` set
/// (`app.kubernetes.io/managed-by=owui-broker`).
pub const MANAGED_BY_SELECTOR: &str = "app.kubernetes.io/managed-by=owui-broker";

/// What the reaper decided to do with one sandbox this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaperAction {
    /// Under every threshold (or no last-used) — leave alone.
    Skip,
    /// Persistent + idle past park TTL + not already Suspended — set Suspended.
    Park,
    /// Ephemeral past idle TTL, or persistent past reap TTL — delete (offload first).
    Reap,
}

/// Per-tick counters (the leader-view gauge surface in C-6 will read off these).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaperStats {
    /// Sandboxes the selector returned this tick.
    pub scanned: u64,
    /// Sandboxes left untouched (under threshold, no last-used, or a failed action).
    pub skipped: u64,
    /// Persistent sandboxes flipped to `Suspended`.
    pub parked: u64,
    /// Sandboxes deleted (reaped).
    pub reaped: u64,
    /// Reaps deferred because the offload hook failed (D7 keep-alive).
    pub offload_failed: u64,
    /// s3-record (#207): suspended reaps released without a snapshot because
    /// the offload could not run — fired on ANY offload failure for a
    /// suspended sandbox (no serving pod to resume, or an S3 outage) while
    /// a re-check still finds it `Suspended`; holding a suspended sandbox
    /// achieves nothing. A sandbox the offloader already resumed keeps the
    /// ordinary D7 keep-alive instead and is NOT counted here.
    pub suspended_offload_unavailable: u64,
    /// Terminating sandboxes whose deletion stayed blocked this tick: the
    /// termination offload failed, or the finalizer release failed.
    pub termination_blocked: u64,
}

/// Decide the action for one sandbox from its profile, idle time, current mode,
/// and the reaper config. Pure + deterministic — the unit-test surface for the
/// idle policy (ephemeral→reap, persistent→park/reap, under-threshold→skip).
fn decide(
    profile: Profile,
    idle_secs: u64,
    current_mode: OperatingMode,
    cfg: &shared::BrokerConfig,
) -> ReaperAction {
    match profile {
        // empty-dir hot tier — reap at IDLE_TTL (like ephemeral) so the
        // cold tier (S3) captures state before the emptyDir is destroyed.
        // The reaper offloads first (C-4 ReapOffload); never parks (the pod
        // must be alive to snapshot). Keyed on the HOT tier, not on
        // s3_enabled: PVC × S3 hybrids park/reap normally (#142) — their
        // data survives pod delete, and reap-of-parked resumes to offload.
        Profile::Persistent
            if matches!(cfg.persistent_mode, shared::PersistentMode::EmptyDir)
                && idle_secs > cfg.idle_ttl_seconds =>
        {
            ReaperAction::Reap
        }
        Profile::Persistent if matches!(cfg.persistent_mode, shared::PersistentMode::EmptyDir) => {
            ReaperAction::Skip
        }
        // persistent (non-s3-tiered) — park at PARK_TTL, reap at REAP_TTL.
        // Reap takes precedence; an already-Suspended sandbox is never re-parked.
        Profile::Persistent if idle_secs > cfg.reap_seconds => ReaperAction::Reap,
        // s3-record (#207): past the park window EVERY s3-record sandbox is a
        // reap candidate, whatever its mode — the store of record replaces
        // both parked states. A Suspended sandbox only pins its scratch
        // storage (or a warm-pool member), and parking a Running one would
        // suspend it into that same pin. Keeping the whole s3-record policy
        // here makes decide() the single policy surface. PVC hot tiers keep
        // Suspended as a valid parked state (the PVC holds the data).
        Profile::Persistent
            if cfg.persistent_mode.is_s3_record() && idle_secs > cfg.park_idle_seconds =>
        {
            ReaperAction::Reap
        }
        Profile::Persistent
            if idle_secs > cfg.park_idle_seconds && current_mode != OperatingMode::Suspended =>
        {
            ReaperAction::Park
        }
        Profile::Persistent => ReaperAction::Skip,
        // ephemeral (emptyDir) — reap at IDLE_TTL (return capacity to pool).
        Profile::Ephemeral if idle_secs > cfg.idle_ttl_seconds => ReaperAction::Reap,
        Profile::Ephemeral => ReaperAction::Skip,
    }
}

/// Read the `broker-profile` label, defaulting to ephemeral when absent or
/// unrecognised (an unknown value falls through as ephemeral).
fn profile_of(labels: Option<&BTreeMap<String, String>>) -> Profile {
    match labels
        .and_then(|l| l.get(PROFILE_LABEL_KEY))
        .map(String::as_str)
    {
        Some("persistent") => Profile::Persistent,
        _ => Profile::Ephemeral,
    }
}

/// Parse the `broker-last-used` annotation to epoch seconds (defaulting to
/// 0). `None` when absent/unparseable;
/// callers treat `None` **and** `Some(0)` as "no last-used → skip".
fn last_used_of(annotations: &BTreeMap<String, String>) -> Option<i64> {
    annotations
        .get(LAST_USED_KEY)
        .and_then(|v| v.parse::<i64>().ok())
}

/// Whether the s3-snapshot finalizer is present on the sandbox.
fn has_finalizer(sbx: &Sandbox, finalizer: &str) -> bool {
    sbx.metadata
        .finalizers
        .as_ref()
        .is_some_and(|f| f.iter().any(|v| v == finalizer))
}

/// Snapshot ts embedded in a last-offloaded-key stamp
/// (`<ns>/workspace-<10-digit ts>.tar.zst`, zero-padded so the ts is fixed
/// width). `None` when the key shape is unrecognised — never dedupe on it.
fn stamp_ts(key: &str) -> Option<i64> {
    let file = key.rsplit('/').next().unwrap_or(key);
    let ts = file.strip_prefix("workspace-")?.strip_suffix(".tar.zst")?;
    ts.parse().ok()
}

/// Fresh-snapshot dedupe: the `S3_LAST_KEY_ANNOTATION` stamp names a snapshot
/// taken at/after the sandbox's last recorded activity — no workspace change
/// since the archive, so a second offload would rewrite identical content.
/// Any missing/unparseable input ⇒ not fresh (the safe side: offload again).
fn fresh_snapshot(sbx: &Sandbox) -> bool {
    let Some(annots) = sbx.metadata.annotations.as_ref() else {
        return false;
    };
    let Some(key) = annots.get(S3_LAST_KEY_ANNOTATION) else {
        return false;
    };
    let Some(ts) = stamp_ts(key) else {
        return false;
    };
    last_used_of(annots).is_some_and(|lu| ts >= lu)
}

/// Remove the s3-snapshot finalizer; a failed removal keeps the deletion
/// blocked (counted so ops can alert on it).
async fn release_finalizer(store: &dyn SandboxStore, name: &str, stats: &mut ReaperStats) {
    if let Err(e) = store
        .remove_finalizer(name, shared::FINALIZER_S3_SNAPSHOT)
        .await
    {
        stats.termination_blocked += 1;
        tracing::warn!(%name, error = %e, "finalizer removal failed; deletion stays blocked");
        return;
    }
    tracing::info!(%name, "s3-snapshot finalizer released; deletion unblocked");
}

/// One Terminating (`deletionTimestamp` set) sandbox: run the s3-record
/// snapshot contract — offload unless a fresh snapshot already covers the
/// workspace, then release the finalizer so the apiserver finishes the
/// delete. A failing offload HOLDS the finalizer (deletion stays blocked
/// until a later tick succeeds).
async fn handle_terminating(
    store: &dyn SandboxStore,
    offload: &dyn ReapOffload,
    cfg: &shared::BrokerConfig,
    sbx: &Sandbox,
    stats: &mut ReaperStats,
) {
    let name = sbx.name_any();
    if !has_finalizer(sbx, shared::FINALIZER_S3_SNAPSHOT) {
        return; // not ours (or already released) — nothing blocks the delete
    }
    if !cfg.termination_finalizer_active() {
        // Knob off / non-s3 mode: never block a deletion for a snapshot that
        // will never be taken.
        release_finalizer(store, &name, stats).await;
        return;
    }
    if fresh_snapshot(sbx) {
        tracing::debug!(%name, "fresh snapshot stamp; termination offload skipped");
        release_finalizer(store, &name, stats).await;
        return;
    }
    match offload.offload_on_reap(sbx).await {
        Ok(()) => release_finalizer(store, &name, stats).await,
        Err(e) => {
            stats.termination_blocked += 1;
            tracing::warn!(
                %name, error = %e,
                "termination offload failed; finalizer held, deletion blocked"
            );
        }
    }
}

/// Current epoch seconds (never panics on a pre-epoch clock).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The C-4 cold-tier offload seam: stream a sandbox's `/workspace` to cold
/// storage before it is reaped. C-3's [`NoopOffload`] is a logged no-op that
/// always succeeds; C-4 swaps in the S3-tiered offload.
#[async_trait]
pub trait ReapOffload: Send + Sync {
    /// Offload `sandbox`'s workspace. Returning [`OffloadError`] keeps the
    /// sandbox alive for the next reaper tick (D7 — never silently lose a
    /// snapshot); the no-op default never errors.
    async fn offload_on_reap(&self, sandbox: &Sandbox) -> Result<(), OffloadError>;
}

/// Why an offload was deferred (surfaced to the reaper so it can keep-alive).
#[derive(Debug, thiserror::Error)]
pub enum OffloadError {
    /// The cold-tier write failed (transport, auth, HTTP non-2xx, …). The reaper
    /// retries next tick; the sandbox is NOT deleted.
    #[error("offload failed: {0}")]
    Failed(String),
}

/// C-3 default offload: do nothing (S3 tiering lands in C-4). Always succeeds,
/// so the reaper deletes immediately — correct for ephemeral + per-user-pvc
/// sandboxes, which carry nothing to offload (only s3-tiered is offloaded).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopOffload;

#[async_trait]
impl ReapOffload for NoopOffload {
    async fn offload_on_reap(&self, sandbox: &Sandbox) -> Result<(), OffloadError> {
        tracing::debug!(
            name = %sandbox.name_any(),
            "offload hook: no-op (C-4 will implement S3 offload)"
        );
        Ok(())
    }
}

/// Post-offload release for a Reap decision: s3-record finalizer handoff,
/// claim-first delete (cascade), metrics + runtime-key cleanup. Shared by the
/// clean-offload path and the s3-record suspended release (offload
/// unavailable, #207).
async fn release_reaped(
    store: &dyn SandboxStore,
    cfg: &shared::BrokerConfig,
    sbx: &Sandbox,
    profile: Profile,
    idle: u64,
    name: &str,
    stats: &mut ReaperStats,
) {
    // s3-record + finalizer: release BEFORE the delete so the apiserver
    // completes it immediately (the stamp is fresh — a later dedupe tick
    // would hit anyway). A failed release only lingers the object
    // Terminating; the next tick's dedupe clears it.
    if has_finalizer(sbx, shared::FINALIZER_S3_SNAPSHOT) && cfg.termination_finalizer_active() {
        if let Err(e) = store
            .remove_finalizer(name, shared::FINALIZER_S3_SNAPSHOT)
            .await
        {
            tracing::warn!(
                %name, error = %e,
                "finalizer release before reap failed; object lingers Terminating"
            );
        }
    }
    // Claimed ephemeral (warm-pool): delete the CLAIM — cascade removes the
    // bound Sandbox + pod and the pool replenishes. Fall back to deleting the
    // Sandbox when the claim is already gone (orphan cleanup).
    let claim = sbx
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crate::sandbox::CLAIM_ANNOTATION_KEY))
        .map(String::as_str);
    let deleted = match claim {
        // Claim present — delete it; cascade removes the Sandbox + pod and
        // the pool replenishes. A claim already gone means an orphaned
        // sandbox: delete it.
        Some(c) => match store.delete_claim(c).await {
            Ok(true) => {
                tracing::info!(
                    %name, claim = %c, idle,
                    "reaped idle claim (sandbox released, pool replenishes)"
                );
                true
            }
            Ok(false) => match store.delete_sandbox(name).await {
                Ok(v) => v,
                Err(e) => {
                    stats.skipped += 1;
                    tracing::warn!(%name, error = %e, "reap delete failed");
                    return;
                }
            },
            Err(e) => {
                stats.skipped += 1;
                tracing::warn!(%name, claim = %c, error = %e, "reap claim delete failed");
                return;
            }
        },
        None => match store.delete_sandbox(name).await {
            Ok(v) => v,
            Err(e) => {
                stats.skipped += 1;
                tracing::warn!(%name, error = %e, "reap delete failed");
                return;
            }
        },
    };
    if deleted {
        stats.reaped += 1;
        tracing::info!(%name, idle, "reaped idle sandbox (deleted)");
        // #99 A6: why this sandbox was reaped (mirrors decide()'s reap
        // conditions) — drives idle_reaps_total{reason}.
        let reason = match profile {
            Profile::Ephemeral => "ephemeral_idle",
            // Mirrors decide(): only the empty-dir hot tier reaps at IDLE_TTL
            // (the cold tier is its only durability). PVC hybrids reap at
            // REAP_TTL.
            Profile::Persistent
                if matches!(cfg.persistent_mode, shared::PersistentMode::EmptyDir) =>
            {
                "cold_tier_idle"
            }
            Profile::Persistent => "persistent_reap_ttl",
        };
        metrics::counter!(IDLE_REAPS_TOTAL, "reason" => reason).increment(1);
        // PR-C-5 / #4: best-effort reap the per-session key Secret (absent
        // for claimed ephemeral — 404-tolerant).
        if let Err(e) = store.delete_runtime_key(name).await {
            tracing::warn!(%name, error = %e, "reap runtime key failed");
        }
    } else {
        stats.skipped += 1; // already gone (404)
    }
}

/// One leader-agnostic reaper tick: list broker-owned sandboxes, decide each
/// one's action, apply it. The injectable `now` makes idle time deterministic
/// in tests. Does NOT check the leader gate — see [`maybe_reap_once`].
pub async fn reap_once(
    store: &dyn SandboxStore,
    offload: &dyn ReapOffload,
    cfg: &shared::BrokerConfig,
    now: i64,
) -> ReaperStats {
    let mut stats = ReaperStats::default();
    let sandboxes = match store.list_sandboxes(Some(MANAGED_BY_SELECTOR)).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "reaper: list sandboxes failed");
            return stats;
        }
    };
    stats.scanned = sandboxes.len() as u64;

    for sbx in &sandboxes {
        let name = sbx.name_any();
        // Terminating (deletionTimestamp set): the finalizer contract owns
        // the exit — snapshot (or dedupe) first, release last. Runs before
        // the idle logic so a terminating sandbox is never parked/re-reaped.
        if sbx.metadata.deletion_timestamp.is_some() {
            handle_terminating(store, offload, cfg, sbx, &mut stats).await;
            continue;
        }
        // No usable last-used → skip (treat absent as "not yet tracked").
        let lu = sbx.metadata.annotations.as_ref().and_then(last_used_of);
        let Some(lu) = lu else {
            stats.skipped += 1;
            continue;
        };
        if lu == 0 {
            stats.skipped += 1;
            continue;
        }
        let idle = (now - lu).max(0) as u64;
        let profile = profile_of(sbx.metadata.labels.as_ref());
        let mode = sbx.spec.operating_mode.unwrap_or(OperatingMode::Running);
        let action = decide(profile, idle, mode, cfg);
        match action {
            ReaperAction::Skip => stats.skipped += 1,
            ReaperAction::Park => match store
                .patch_operating_mode(&name, OperatingMode::Suspended)
                .await
            {
                Ok(()) => {
                    stats.parked += 1;
                    tracing::info!(%name, idle, "parked idle sandbox (Suspended)");
                }
                Err(e) => {
                    stats.skipped += 1;
                    tracing::warn!(%name, error = %e, "park failed");
                }
            },
            ReaperAction::Reap => {
                // C-4 seam: offload first; on failure keep-alive for the next
                // tick (D7). s3-record exception (#207): a still-Suspended
                // candidate has no pod that can serve an offload and nothing
                // else will resume it — holding it achieves nothing, so
                // surface the degraded case with its own stat and release it
                // without a snapshot. The release is GATED on a re-fetch:
                // the offloader may have legitimately resumed the sandbox
                // (patch Running + boot succeeded) before its S3 put failed —
                // then the ordinary D7 keep-alive applies, never a release
                // (and never the finalizer strip inside `release_reaped`).
                let suspended_release =
                    cfg.persistent_mode.is_s3_record() && mode == OperatingMode::Suspended;
                match offload.offload_on_reap(sbx).await {
                    Ok(()) => {
                        release_reaped(store, cfg, sbx, profile, idle, &name, &mut stats).await;
                    }
                    Err(e) if suspended_release => match store.get_sandbox(&name).await {
                        // Still Suspended after the failed offload — the
                        // #207 degraded release: no snapshot can be taken,
                        // so release without one (surfaced via the stat).
                        Ok(Some(current))
                            if current.spec.operating_mode == Some(OperatingMode::Suspended) =>
                        {
                            stats.suspended_offload_unavailable += 1;
                            tracing::warn!(
                                %name, error = %e,
                                "offload unavailable for suspended s3-record sandbox; releasing without a snapshot"
                            );
                            release_reaped(store, cfg, sbx, profile, idle, &name, &mut stats).await;
                        }
                        // Resumed by the offloader (or already gone) — keep
                        // the ordinary D7 keep-alive contract.
                        Ok(_) => {
                            stats.offload_failed += 1;
                            tracing::warn!(
                                %name, error = %e,
                                "offload failed after resume; keeping sandbox alive for next tick"
                            );
                        }
                        // Cannot confirm still-Suspended → conservative
                        // keep-alive; the gate re-checks next tick.
                        Err(fetch_err) => {
                            stats.offload_failed += 1;
                            tracing::warn!(
                                %name, error = %e, fetch_error = %fetch_err,
                                "offload failed for suspended s3-record sandbox; state re-check failed, keeping alive"
                            );
                        }
                    },
                    Err(e) => {
                        stats.offload_failed += 1;
                        tracing::warn!(
                            %name, error = %e,
                            "offload failed; keeping sandbox alive for next tick"
                        );
                    }
                }
            }
        }
    }
    stats
}

/// Leader-gated reaper tick: a no-op (empty stats, no store mutation) when this
/// broker is not currently the elected leader, otherwise [`reap_once`]. This is
/// the testable seam for the leader-gate ("non-leader skips reaping").
pub async fn maybe_reap_once(
    gate: &LeaderGate,
    store: &dyn SandboxStore,
    offload: &dyn ReapOffload,
    cfg: &shared::BrokerConfig,
    now: i64,
) -> ReaperStats {
    if !gate.is_leader() {
        return ReaperStats::default();
    }
    reap_once(store, offload, cfg, now).await
}

/// Background reaper loop (leader-gated). Runs every `reaper_poll_seconds`;
/// exits cleanly when the leader loop signals shutdown (the leader loop owns
/// this task's lifetime).
pub async fn run_reaper_loop(
    gate: Arc<LeaderGate>,
    store: Arc<dyn SandboxStore>,
    offload: Arc<dyn ReapOffload>,
    cfg: Arc<shared::BrokerConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Floor at 1s so a misconfigured 0 never hot-loops the apiserver.
    let interval = Duration::from_secs(cfg.reaper_poll_seconds.max(1));
    loop {
        let stats =
            maybe_reap_once(&gate, store.as_ref(), offload.as_ref(), &cfg, now_unix()).await;
        if stats.scanned > 0 || stats.parked > 0 || stats.reaped > 0 || stats.offload_failed > 0 {
            tracing::info!(
                scanned = stats.scanned,
                parked = stats.parked,
                reaped = stats.reaped,
                offload_failed = stats.offload_failed,
                termination_blocked = stats.termination_blocked,
                "reaper tick"
            );
        }
        // D9 — leader-view metrics: only the elected leader returns non-empty
        //        stats here (`maybe_reap_once` no-ops off-leader), so we mirror
        //        that guard to avoid a non-leader zeroing the gauge.
        if gate.is_leader() {
            metrics::gauge!(ACTIVE_SANDBOXES).set(stats.scanned as f64);
            if stats.reaped > 0 {
                metrics::counter!(SANDBOXES_DELETED_TOTAL).increment(stats.reaped as u64);
            }
        }
        // Cooperatively await the next tick OR a shutdown signal.
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    tracing::debug!("reaper loop: shutdown sender dropped");
                }
                tracing::info!("reaper loop shutting down");
                return;
            }
            () = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(idle: u64, park: u64, reap: u64) -> shared::BrokerConfig {
        shared::BrokerConfig {
            idle_ttl_seconds: idle,
            park_idle_seconds: park,
            reap_seconds: reap,
            ..Default::default()
        }
    }

    // --- decide(): the pure idle policy ----------------------------------------

    #[test]
    fn ephemeral_under_idle_ttl_is_skipped() {
        let c = cfg(120, 120, 604_800);
        assert_eq!(
            decide(Profile::Ephemeral, 0, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
        assert_eq!(
            decide(Profile::Ephemeral, 119, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
        assert_eq!(
            decide(Profile::Ephemeral, 120, OperatingMode::Running, &c),
            ReaperAction::Skip,
            "strict-greater (not >=): exactly at TTL is still warm"
        );
    }

    #[test]
    fn ephemeral_over_idle_ttl_is_reaped() {
        let c = cfg(120, 120, 604_800);
        assert_eq!(
            decide(Profile::Ephemeral, 121, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
    }

    #[test]
    fn persistent_under_park_ttl_is_skipped() {
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 299, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
    }

    #[test]
    fn persistent_over_park_ttl_is_parked_when_running() {
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 301, OperatingMode::Running, &c),
            ReaperAction::Park
        );
    }

    #[test]
    fn persistent_over_park_ttl_is_skipped_when_already_suspended() {
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 301, OperatingMode::Suspended, &c),
            ReaperAction::Skip,
            "never re-park an already-parked sandbox (mode != Suspended guard)"
        );
    }

    #[test]
    fn persistent_over_reap_ttl_is_reaped_even_if_suspended() {
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 604_801, OperatingMode::Suspended, &c),
            ReaperAction::Reap,
            "reap takes precedence over park, including for parked sandboxes"
        );
        assert_eq!(
            decide(Profile::Persistent, 604_801, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
    }

    #[test]
    fn persistent_reap_precedence_over_park() {
        // idle past BOTH park and reap → reap wins.
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 700_000, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
    }

    // --- decide(): the S3-tiered persistent branch (C-4, issue #52) ----------

    fn cfg_s3(idle: u64, park: u64, reap: u64) -> shared::BrokerConfig {
        shared::BrokerConfig {
            idle_ttl_seconds: idle,
            park_idle_seconds: park,
            reap_seconds: reap,
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::EmptyDir,
            ..Default::default()
        }
    }

    #[test]
    fn empty_dir_hot_tier_reaps_at_idle_ttl_like_ephemeral() {
        // empty-dir hot tier reaps at IDLE_TTL (the cold tier is the only
        // durability) — never waits for PARK/REAP_TTL, so the emptyDir state
        // is captured before the pod is destroyed.
        let c = cfg_s3(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 121, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
        assert_eq!(
            decide(Profile::Persistent, 604_801, OperatingMode::Suspended, &c),
            ReaperAction::Reap
        );
    }

    #[test]
    fn empty_dir_hot_tier_is_never_parked() {
        // The empty-dir branch only ever returns Reap (over IDLE_TTL) or Skip
        // (under) — never Park — so the pod stays alive to snapshot. The
        // PARK_TTL threshold is never consulted (the cold tier is S3).
        let c = cfg_s3(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 60, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
        assert_eq!(
            decide(Profile::Persistent, 400, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
        assert_eq!(
            decide(Profile::Persistent, 400, OperatingMode::Suspended, &c),
            ReaperAction::Reap
        );
    }

    #[test]
    fn empty_dir_hot_tier_under_idle_ttl_is_skipped() {
        let c = cfg_s3(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 120, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
    }

    #[test]
    fn s3_disabled_persistent_still_parks_and_reaps_normally() {
        // A PVC hot tier parks at PARK_TTL and reaps at REAP_TTL with the cold
        // tier off (cfg() defaults s3_enabled off and mode per-user-pvc).
        let c = cfg(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 400, OperatingMode::Running, &c),
            ReaperAction::Park
        );
        assert_eq!(
            decide(Profile::Persistent, 604_801, OperatingMode::Running, &c),
            ReaperAction::Reap
        );
    }

    #[test]
    fn hybrid_pvc_with_s3_still_parks_and_reaps_normally() {
        // #142: tiering is independent of the hot tier. A PVC-backed
        // persistent sandbox parks/reaps on the PVC schedule even with the
        // cold tier enabled — the data survives pod delete, and the reaper
        // resumes a parked sandbox to offload before deleting it.
        for mode in [
            shared::PersistentMode::PerUserPvc,
            shared::PersistentMode::SharedSubpath,
        ] {
            let c = shared::BrokerConfig {
                idle_ttl_seconds: 120,
                park_idle_seconds: 300,
                reap_seconds: 604_800,
                s3_enabled: true,
                persistent_mode: mode,
                ..Default::default()
            };
            assert_eq!(
                decide(Profile::Persistent, 121, OperatingMode::Running, &c),
                ReaperAction::Skip,
                "hybrid must not reap at IDLE_TTL (that is empty-dir behaviour)"
            );
            assert_eq!(
                decide(Profile::Persistent, 400, OperatingMode::Running, &c),
                ReaperAction::Park
            );
            assert_eq!(
                decide(Profile::Persistent, 604_801, OperatingMode::Suspended, &c),
                ReaperAction::Reap
            );
        }
    }

    #[test]
    fn s3_record_suspended_past_park_window_is_a_reap_candidate() {
        // #207: s3-record has no Suspended state to keep alive — a parked
        // sandbox past the park window is a reap candidate (archive-if-
        // possible → release) even though it can never be re-parked.
        let c = shared::BrokerConfig {
            idle_ttl_seconds: 120,
            park_idle_seconds: 300,
            reap_seconds: 604_800,
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::S3Record,
            ..Default::default()
        };
        assert_eq!(
            decide(Profile::Persistent, 301, OperatingMode::Suspended, &c),
            ReaperAction::Reap
        );
        assert_eq!(
            decide(Profile::Persistent, 299, OperatingMode::Suspended, &c),
            ReaperAction::Skip,
            "under the park window a suspended sandbox is still left alone"
        );
    }

    #[test]
    fn s3_record_running_past_park_window_is_a_reap_candidate_not_parked() {
        // The Park→Reap collapse lives in decide(): a Running s3-record
        // member past the park window comes back as Reap, so decide()
        // alone is the idle-policy surface (no reap_once rewriting).
        let c = shared::BrokerConfig {
            idle_ttl_seconds: 120,
            park_idle_seconds: 300,
            reap_seconds: 604_800,
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::S3Record,
            ..Default::default()
        };
        assert_eq!(
            decide(Profile::Persistent, 301, OperatingMode::Running, &c),
            ReaperAction::Reap,
            "park+1 Running member is a reap candidate in s3-record"
        );
    }

    #[test]
    fn per_user_pvc_suspended_past_park_window_stays_parked() {
        // #207: the reap-candidate branch is mode-gated — per-user-pvc keeps
        // Suspended as a valid parked state (the PVC holds the data), even
        // with the cold tier enabled.
        let c = shared::BrokerConfig {
            idle_ttl_seconds: 120,
            park_idle_seconds: 300,
            reap_seconds: 604_800,
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::PerUserPvc,
            ..Default::default()
        };
        assert_eq!(
            decide(Profile::Persistent, 301, OperatingMode::Suspended, &c),
            ReaperAction::Skip
        );
    }

    // --- annotation/label parsing -------------------------------------------

    #[test]
    fn profile_defaults_to_ephemeral_when_label_absent_or_unknown() {
        let mut labels = BTreeMap::new();
        assert_eq!(profile_of(None), Profile::Ephemeral);
        assert_eq!(profile_of(Some(&labels)), Profile::Ephemeral);
        labels.insert(PROFILE_LABEL_KEY.to_string(), "ephemeral".into());
        assert_eq!(profile_of(Some(&labels)), Profile::Ephemeral);
        labels.insert(PROFILE_LABEL_KEY.to_string(), "garbage".into());
        assert_eq!(profile_of(Some(&labels)), Profile::Ephemeral);
        labels.insert(PROFILE_LABEL_KEY.to_string(), "persistent".into());
        assert_eq!(profile_of(Some(&labels)), Profile::Persistent);
    }

    #[test]
    fn last_used_parses_epoch_and_skips_garbage() {
        let mut a = BTreeMap::new();
        assert_eq!(last_used_of(&a), None);
        a.insert(LAST_USED_KEY.into(), "not-a-number".into());
        assert_eq!(last_used_of(&a), None);
        a.insert(LAST_USED_KEY.into(), "1700000000".into());
        assert_eq!(last_used_of(&a), Some(1_700_000_000));
        a.insert(LAST_USED_KEY.into(), "0".into());
        assert_eq!(last_used_of(&a), Some(0), "0 parses but the loop skips it");
    }
}
