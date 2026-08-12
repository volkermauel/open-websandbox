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
        // s3-tiered persistent — reap at IDLE_TTL (like ephemeral) so
        // the cold tier (S3) captures state before the emptyDir is destroyed.
        // The reaper offloads first (C-4 ReapOffload); never parks (the pod
        // must be alive to snapshot). Gated on `s3_enabled`.
        Profile::Persistent if cfg.s3_enabled && idle_secs > cfg.idle_ttl_seconds => {
            ReaperAction::Reap
        }
        Profile::Persistent if cfg.s3_enabled => ReaperAction::Skip,
        // persistent (non-s3-tiered) — park at PARK_TTL, reap at REAP_TTL.
        // Reap takes precedence; an already-Suspended sandbox is never re-parked.
        Profile::Persistent if idle_secs > cfg.reap_seconds => ReaperAction::Reap,
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
        match decide(profile, idle, mode, cfg) {
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
                // C-4 seam: offload first; on failure keep-alive for the next tick (D7).
                match offload.offload_on_reap(sbx).await {
                    Ok(()) => match store.delete_sandbox(&name).await {
                        Ok(true) => {
                            stats.reaped += 1;
                            tracing::info!(%name, idle, "reaped idle sandbox (deleted)");
                            // #99 A6: why this sandbox was reaped (mirrors decide()'s
                            // reap conditions) — drives idle_reaps_total{reason}.
                            let reason = match profile {
                                Profile::Ephemeral => "ephemeral_idle",
                                Profile::Persistent if cfg.s3_enabled => "s3_tiered_idle",
                                Profile::Persistent => "persistent_reap_ttl",
                            };
                            metrics::counter!(IDLE_REAPS_TOTAL, "reason" => reason).increment(1);
                            // PR-C-5 / #4: best-effort reap the per-session key Secret.
                            if let Err(e) = store.delete_runtime_key(&name).await {
                                tracing::warn!(%name, error = %e, "reap runtime key failed");
                            }
                        }
                        Ok(false) => stats.skipped += 1, // already gone (404)
                        Err(e) => {
                            stats.skipped += 1;
                            tracing::warn!(%name, error = %e, "reap delete failed");
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
            ..Default::default()
        }
    }

    #[test]
    fn s3_tiered_persistent_reaps_at_idle_ttl_like_ephemeral() {
        // s3-tiered reaps at IDLE_TTL (cold tier is S3) — never waits
        // for PARK/REAP_TTL — so the emptyDir state is captured before destroy.
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
    fn s3_tiered_persistent_is_never_parked() {
        // The s3-tiered branch only ever returns Reap (over IDLE_TTL) or Skip
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
    fn s3_tiered_persistent_under_idle_ttl_is_skipped() {
        let c = cfg_s3(120, 300, 604_800);
        assert_eq!(
            decide(Profile::Persistent, 120, OperatingMode::Running, &c),
            ReaperAction::Skip
        );
    }

    #[test]
    fn s3_disabled_persistent_still_parks_and_reaps_normally() {
        // The s3-tiered branch only applies when s3_enabled (cfg() defaults it
        // off): a PVC-persistent sandbox parks at PARK_TTL, reaps at REAP_TTL.
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
