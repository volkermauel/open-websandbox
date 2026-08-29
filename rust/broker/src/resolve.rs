//! Deterministic session→sandbox resolution + ready-polling.
//!
//! Per-session sandbox resolution (PR-C-2): compute the deterministic per-session
//! Sandbox name, get-or-create it via the `SandboxStore`, then poll the store
//! until the upstream controller reports `Ready` **and** a pod IP (reusing
//! [`SandboxStatus::is_ready`](shared::SandboxStatus::is_ready) +
//! [`pod_ip`](shared::SandboxStatus::pod_ip)).
//!
//! Out of scope here (land in later PRs): per-session
//! runtime-key mint/rotate (C-3), staging→chat migration (C-3), S3-tiered restore
//! (C-4). The controller-set interaction (park/resume) is C-3/leader-election
//! territory, so a present-but-not-Ready sandbox is simply polled here.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use shared::{PersistentMode, Profile, Sandbox, SandboxStatus};

use crate::error::ApiError;
use crate::metrics::SANDBOXES_CREATED_TOTAL;
use crate::sandbox::{apply_persistent_volume, build_sandbox, extract_pod_template};
use crate::state::AppState;
use crate::store::{StoreError, WorkspacePvcSpec};

/// Prefix for ephemeral (per-SESSION, emptyDir) sandboxes.
pub const EPHEMERAL_PREFIX: &str = "owui-";
/// Prefix for persistent (per-CHAT) sandboxes.
pub const CHAT_PREFIX: &str = "owui-c-";

/// Poll interval while waiting for a Sandbox to reach `Ready`. PR-C-2 polls
/// the store (simpler, fine for C-2 per
/// the issue). 250 ms keeps perceived latency low without hammering the apiserver
/// on the real backend.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A resolved, ready sandbox: its name (== pod name == runtime-key Secret owner)
/// and the pod IP the broker proxies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandbox {
    /// Sandbox (== pod) name.
    pub name: String,
    /// Pod IP the runtime is serving on (`:8888`).
    pub pod_ip: String,
}

/// Deterministic, DNS-label-safe per-session Sandbox name.
///
/// * Ephemeral profile: `owui-` + `sha256("{user}|{session}")[:12]`.
/// * Persistent profile: `owui-c-` + `sha256("{user}/{session}")[:12]`.
///
/// The scheme is byte-identical to the reference broker — including the differing
/// separator (`|` vs `/`) and prefix (`owui-` vs `owui-c-`) — so any broker
/// instance resolves the SAME object for a given user/session (D11 cutover
/// safety).
#[must_use]
pub fn sandbox_name(user_id: &str, session_id: &str, profile: Profile) -> String {
    match profile {
        Profile::Persistent => {
            let digest = hex12(format!("{user_id}/{session_id}").as_bytes());
            format!("{CHAT_PREFIX}{digest}")
        }
        Profile::Ephemeral => {
            let digest = hex12(format!("{user_id}|{session_id}").as_bytes());
            format!("{EPHEMERAL_PREFIX}{digest}")
        }
    }
}

/// First 12 hex chars (6 bytes) of `sha256(input)` (`hexdigest()[:12]`).
fn hex12(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let out = hasher.finalize();
    out.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// PVC claim name + per-chat subPath for a persistent sandbox (#140).
///
/// PVC granularity is the **user** (quota/reclaim), subPath granularity is
/// the **chat** (isolation — a chat's terminal only ever sees its own
/// directory). Every path component is a `sha256` hex prefix, so raw
/// `X-User-Id` / `X-Session-Id` bytes never reach a volume name or a path
/// (no traversal, no invalid characters), and every broker replica computes
/// the identical layout for a given (user, session):
///
/// * `per-user-pvc`  → (`<prefix><sha256(user)[:12]>`, `chats/<sha256(user/session)[:12]>`)
/// * `shared-subpath` → (`<sharedPvc>`, `users/<sha256(user)[:12]>/chats/<sha256(user/session)[:12]>`)
fn workspace_layout(
    config: &shared::BrokerConfig,
    user_id: &str,
    session_id: &str,
) -> (String, String) {
    let user = hex12(user_id.as_bytes());
    let chat = hex12(format!("{user_id}/{session_id}").as_bytes());
    match config.persistent_mode {
        PersistentMode::SharedSubpath => (
            config.shared_pvc_name.clone(),
            format!("users/{user}/chats/{chat}"),
        ),
        // `EmptyDir` never reaches the PVC block (is_pvc() gate at the call
        // site); the match stays exhaustive, its layout is unused.
        PersistentMode::PerUserPvc | PersistentMode::EmptyDir => (
            format!("{}{user}", config.per_user_pvc_prefix),
            format!("chats/{chat}"),
        ),
    }
}

/// Pod IP of a `Ready` sandbox, or `None`.
fn ready_pod_ip(sbx: &Sandbox) -> Option<String> {
    let status: &SandboxStatus = sbx.status.as_ref()?;
    if status.is_ready() {
        status.pod_ip().map(str::to_owned)
    } else {
        None
    }
}

/// Map a [`StoreError`] onto an [`ApiError`] for the resolve path.
fn map_store_err(err: StoreError) -> ApiError {
    match err {
        StoreError::NotFound => ApiError::NotFound("not found".to_string()),
        StoreError::Conflict => ApiError::Conflict("already exists".to_string()),
        StoreError::Kube(e) => ApiError::BadGateway(e.to_string()),
    }
}

/// Current epoch seconds (never panics on a pre-epoch clock).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Resolve the per-session Sandbox: get-or-create, then poll until Ready + IP.
///
/// 1. Compute the deterministic [`sandbox_name`].
/// 2. `SandboxStore::get_sandbox`; if absent, clone the base template's
///    `podTemplate` via [`build_sandbox`] and `SandboxStore::create_sandbox`.
///    A `Conflict` means a concurrent create won — fall through to the poll.
/// 3. Poll `SandboxStore::get_sandbox` every `READY_POLL_INTERVAL` until
///    `ready_pod_ip` or the configured `claim_timeout_seconds` deadline
///    (→ 503, matching the PR-C-2 spec).
///
/// Returns the ready [`ResolvedSandbox`] (name + pod IP).
#[tracing::instrument(name = "sandbox.resolve", skip(state), fields(sandbox = tracing::field::Empty))]
pub async fn resolve_sandbox(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    profile: Profile,
) -> Result<ResolvedSandbox, ApiError> {
    let name = sandbox_name(user_id, session_id, profile);
    tracing::Span::current().record("sandbox", name.as_str());

    // #157 draft adoption: planned in the create branch below, executed
    // after readiness (just before `Ok(resolved)`) so the first file listing
    // already sees the adopted files.
    let mut draft_adoption: Option<crate::store::WorkspaceMove> = None;
    // --- get-or-create -----------------------------------------------------
    let existing = state
        .store
        .get_sandbox(&name)
        .await
        .map_err(map_store_err)?;
    if existing.is_none() {
        let template = state
            .store
            .get_template(&state.config.base_template)
            .await
            .map_err(map_store_err)?
            .ok_or_else(|| {
                ApiError::Internal(format!(
                    "base template {} not found",
                    state.config.base_template
                ))
            })?;
        let mut pod_template = extract_pod_template(&template)?;

        // PVC hot tiers (#140): ensure the backing PVC exists, then repoint
        // the cloned pod template's `workspace` volume at it with the
        // per-chat subPath. s3-tiered keeps its emptyDir hot tier (the S3
        // restore below handles persistence) and skips this block.
        if profile == Profile::Persistent && state.config.persistent_mode.is_pvc() {
            let (claim, sub_path) = workspace_layout(&state.config, user_id, session_id);
            let create = (state.config.persistent_mode == PersistentMode::PerUserPvc).then(|| {
                WorkspacePvcSpec {
                    access_modes: state.config.persistent_access_modes.clone(),
                    storage: state.config.persistent_storage.clone(),
                    storage_class: state.config.persistent_storage_class.clone(),
                }
            });
            state
                .store
                .ensure_workspace_pvc(&claim, create.as_ref())
                .await
                .map_err(|e| match e {
                    StoreError::NotFound => ApiError::Internal(format!(
                        "persistentMode=shared-subpath but PVC '{claim}' not found — install the chart with sharedPvc configured",
                    )),
                    other => map_store_err(other),
                })?;
            apply_persistent_volume(&mut pod_template, &claim, &sub_path)?;
        }
        // #157: a fresh chat sandbox may adopt the user's draft workspace
        // (pre-first-message uploads) — plan the move now, run it after the
        // sandbox is ready. Placed before `build_sandbox` because the pod
        // template is moved into the Sandbox below.
        draft_adoption =
            capture_draft_adoption(state, user_id, session_id, &name, &pod_template, profile).await;

        // PR-C-5 / #4: ensure the per-session runtime-key Secret exists BEFORE
        // the Sandbox is created, so the non-optional runtime-key volume is
        // satisfiable when the controller schedules the pod. Fail-fast: a missing
        // key would CrashLoop the runtime (fail-closed boot guard).
        state
            .store
            .ensure_runtime_key(&name)
            .await
            .map_err(map_store_err)?;
        let mut sandbox = build_sandbox(
            &name,
            Some(user_id),
            Some(session_id),
            profile,
            pod_template,
            &state.config.runtime_ns,
            now_unix(),
        );
        // #140: stamp the hot-tier mode onto the Sandbox for ops
        // (`kubectl get sandbox -l broker-persistent-mode=shared-subpath`).
        if profile == Profile::Persistent {
            if let Some(labels) = sandbox.metadata.labels.as_mut() {
                labels.insert(
                    crate::sandbox::PERSISTENT_MODE_LABEL_KEY.to_string(),
                    state.config.persistent_mode.as_str().to_string(),
                );
            }
        }

        // #157: persist the adoption intent on the Sandbox itself — a claim
        // attempt can time out before readiness (slow first boot) and the
        // in-memory plan dies with it; the pending marker lets any LATER
        // resolve of this chat rebuild and run the move (one-shot: cleared
        // once the move ran).
        if draft_adoption.is_some() {
            if let Some(annots) = sandbox.metadata.annotations.as_mut() {
                annots.insert(
                    crate::sandbox::DRAFT_ADOPT_PENDING_KEY.to_string(),
                    sandbox_name(user_id, user_id, Profile::Persistent),
                );
            }
        }
        match state.store.create_sandbox(sandbox).await {
            Ok(_) => {
                // D9: a new sandbox was actually created (resolve path).
                metrics::counter!(SANDBOXES_CREATED_TOTAL).increment(1);
            }
            // A concurrent resolve won the create race — poll for the winner.
            Err(StoreError::Conflict) => {}
            Err(e) => return Err(map_store_err(e)),
        }
    }
    // Resume a parked sandbox before the ready poll: flip a `Suspended`
    // sandbox back to `Running` so its pod schedules
    // before the Ready poll. C-3's rotate-on-resume (per-session key) stays
    // deferred to the per-session-key PR; C-4 only needs the operatingMode flip
    // + the restore below.
    let needs_resume = existing.as_ref().and_then(|s| s.spec.operating_mode)
        == Some(shared::OperatingMode::Suspended);
    if needs_resume {
        if let Err(e) = state
            .store
            .patch_operating_mode(&name, shared::OperatingMode::Running)
            .await
        {
            tracing::warn!(sandbox = %name, error = %e, "resume operatingMode patch failed");
        } else {
            // #150: restart the idle clock the moment we ask a parked sandbox
            // to resume. last-used is otherwise only touched AFTER readiness —
            // a slow resume with a stale clock let the leader reaper re-park
            // the sandbox mid-boot (digest-verified: "not ready in 60s
            // (… Suspended reason=PodTerminated … SandboxSuspended)"), and the
            // resolve could never win the race.
            let _ = state.store.touch_last_used(&name, now_unix()).await;
        }
    }

    let resolved = wait_for_ready(state, &name).await?;
    // Activity bump: refresh `broker-last-used` so the
    // leader's reaper doesn't park/reap a sandbox mid-session. Best-effort — the
    // resolve already succeeded, so a patch failure is logged, never fatal.
    if let Err(e) = state
        .store
        .touch_last_used(&resolved.name, now_unix())
        .await
    {
        tracing::debug!(sandbox = %resolved.name, error = %e, "non-fatal last-used touch");
    }
    // C-4 restore-on-resume (gated on S3 tiering +
    // persistent profile): block readiness until S3 → /workspace is present.
    // No-op on first creation (no object under the namespace); on restore
    // failure we FAIL the resume (502) so the user never gets an empty
    // workspace (D7).
    if profile == Profile::Persistent {
        if let Some(tier) = state.s3_restore.clone() {
            match tier
                .restore_on_resume(&resolved.name, &resolved.pod_ip, user_id, session_id)
                .await
            {
                Ok(crate::s3::RestoreOutcome::Restored(key)) => {
                    tracing::info!(
                        sandbox = %resolved.name, key = %key,
                        "s3 restore-on-resume complete"
                    );
                }
                Ok(crate::s3::RestoreOutcome::NoObject) => {
                    tracing::debug!(
                        sandbox = %resolved.name,
                        "s3 restore skipped (no object — first creation)"
                    );
                }
                Ok(crate::s3::RestoreOutcome::HotTierHit) => {
                    tracing::debug!(
                        sandbox = %resolved.name,
                        "s3 restore skipped (workspace non-empty — hot-tier hit)"
                    );
                }
                Err(e) => {
                    return Err(ApiError::BadGateway(format!("s3 restore failed: {e}")));
                }
            }
        }
    }
    // #157 draft adoption: block until the draft→chat move completed so the
    // first file listing already sees the adopted files. Best-effort — a
    // failed move is logged + counted, never fatal (the resolve already
    // succeeded; the fallback is today's behaviour: an empty workspace).
    // The plan comes from THIS resolve's create branch, or — when an earlier
    // attempt timed out before readiness — from the pending marker stamped
    // on the Sandbox (retry-proof; e2e-verified live: a chat whose first
    // claim 503'd used to silently lose the adoption).
    let adoption_move = match draft_adoption {
        Some(mv) => Some(mv),
        None => resume_draft_adoption(state, user_id, session_id, &resolved.name).await,
    };
    if let Some(mv) = adoption_move {
        match state.store.move_workspace_dir(&mv).await {
            Ok(true) => {
                metrics::counter!(crate::metrics::DRAFT_ADOPTIONS_TOTAL, "result" => "adopted")
                    .increment(1);
                tracing::info!(
                    sandbox = %resolved.name,
                    from = %mv.from_subpath,
                    "draft workspace adopted into chat"
                );
            }
            outcome => {
                metrics::counter!(crate::metrics::DRAFT_ADOPTIONS_TOTAL, "result" => "failed")
                    .increment(1);
                tracing::warn!(
                    sandbox = %resolved.name,
                    outcome = ?outcome,
                    "draft adoption failed (continuing with empty workspace)"
                );
            }
        }
        // One-shot marker regardless of outcome: a failed move falls back to
        // the documented empty-workspace behaviour instead of retrying forever.
        if let Err(e) = state
            .store
            .clear_annotation(&resolved.name, crate::sandbox::DRAFT_ADOPT_PENDING_KEY)
            .await
        {
            tracing::debug!(sandbox = %resolved.name, error = %e, "pending-marker clear failed");
        }
    }
    Ok(resolved)
}

/// Plan the #157 draft→chat workspace move for a freshly created chat
/// sandbox, or `None` when adoption doesn't apply: persistent profile on a
/// PVC hot tier, S3 restore not in play (it owns persistence semantics),
/// window enabled, and the user's draft sandbox (`sandbox_name(user, user)`
/// — where OWUI's session-less traffic lands) was last used within the
/// window. Pure preconditions return early so the store is only hit when
/// everything else already lined up.
async fn capture_draft_adoption(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    chat_name: &str,
    pod_template: &serde_json::Value,
    profile: Profile,
) -> Option<crate::store::WorkspaceMove> {
    if state.config.draft_adoption_window_seconds == 0
        || state.s3_restore.is_some()
        || profile != Profile::Persistent
        || !state.config.persistent_mode.is_pvc()
    {
        return None;
    }
    let draft_name = sandbox_name(user_id, user_id, Profile::Persistent);
    if draft_name == chat_name {
        // The caller IS the draft (session-less traffic keyed by user id).
        return None;
    }
    let image = pod_template
        .pointer("/spec/containers/0/image")?
        .as_str()?
        .to_string();
    let ownership = template_pod_ownership(pod_template);
    let draft = match state.store.get_sandbox(&draft_name).await {
        Ok(Some(draft)) => draft,
        Ok(None) => {
            metrics::counter!(crate::metrics::DRAFT_ADOPTIONS_TOTAL, "result" => "skipped_no_draft")
                .increment(1);
            return None;
        }
        Err(_) => return None,
    };
    let last_used = draft
        .metadata
        .annotations
        .as_ref()?
        .get(crate::sandbox::LAST_USED_KEY)?
        .parse::<i64>()
        .ok()?;
    let window = i64::try_from(state.config.draft_adoption_window_seconds).unwrap_or(i64::MAX);
    if now_unix() - last_used > window {
        metrics::counter!(crate::metrics::DRAFT_ADOPTIONS_TOTAL, "result" => "skipped_stale")
            .increment(1);
        return None;
    }
    // Both subpaths live on the same claim (same user).
    let (claim, from_subpath) = workspace_layout(&state.config, user_id, user_id);
    let (_, to_subpath) = workspace_layout(&state.config, user_id, session_id);
    Some(crate::store::WorkspaceMove {
        job_name: format!("draft-adopt-{chat_name}-{}", now_unix()),
        image,
        claim,
        from_subpath,
        to_subpath,
        timeout_secs: 60,
        ownership,
    })
}

/// #157 retry path: an earlier resolve planned an adoption (pending marker
/// stamped on the chat Sandbox) but timed out before readiness — the
/// in-memory `WorkspaceMove` died with that attempt. Any LATER resolve of the
/// same chat rebuilds the move from the marker so the adoption survives
/// claim retries. Same validation as [`capture_draft_adoption`]: the draft
/// must still exist and be fresh within the window.
async fn resume_draft_adoption(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    chat_name: &str,
) -> Option<crate::store::WorkspaceMove> {
    if state.config.draft_adoption_window_seconds == 0
        || state.s3_restore.is_some()
        || !state.config.persistent_mode.is_pvc()
    {
        return None;
    }
    // Only sandboxes that still carry the one-shot marker adopt.
    let chat = state.store.get_sandbox(chat_name).await.ok()??;
    let pending_draft = chat
        .metadata
        .annotations
        .as_ref()?
        .get(crate::sandbox::DRAFT_ADOPT_PENDING_KEY)?
        .clone();
    // Re-validate the draft exactly like the capture path.
    let draft = state.store.get_sandbox(&pending_draft).await.ok()??;
    let last_used = draft
        .metadata
        .annotations
        .as_ref()?
        .get(crate::sandbox::LAST_USED_KEY)?
        .parse::<i64>()
        .ok()?;
    let window = i64::try_from(state.config.draft_adoption_window_seconds).unwrap_or(i64::MAX);
    if now_unix() - last_used > window {
        metrics::counter!(crate::metrics::DRAFT_ADOPTIONS_TOTAL, "result" => "skipped_stale")
            .increment(1);
        return None;
    }
    // Image: the same base template the create branch cloned.
    let template = state
        .store
        .get_template(&state.config.base_template)
        .await
        .ok()??;
    let pod_template = extract_pod_template(&template).ok()?;
    let image = pod_template
        .pointer("/spec/containers/0/image")?
        .as_str()?
        .to_string();
    let ownership = template_pod_ownership(&pod_template);
    // Both subpaths live on the same claim (same user).
    let (claim, from_subpath) = workspace_layout(&state.config, user_id, user_id);
    let (_, to_subpath) = workspace_layout(&state.config, user_id, session_id);
    Some(crate::store::WorkspaceMove {
        job_name: format!("draft-adopt-{chat_name}-{}", now_unix()),
        image,
        claim,
        from_subpath,
        to_subpath,
        timeout_secs: 60,
        ownership,
    })
}
/// Poll the store until the named Sandbox is Ready with a pod IP, or the deadline.
///
/// On timeout the 503 carries a one-line digest of the sandbox's last-seen
/// status — the v0.5.6 controller mirrors `PodScheduled` (Unschedulable,
/// SchedulingGated, …) into `Sandbox.status.conditions`, so the error says
/// *why* the sandbox never came up instead of just "not ready in 60s".
async fn wait_for_ready(state: &AppState, name: &str) -> Result<ResolvedSandbox, ApiError> {
    let deadline = Instant::now() + Duration::from_secs(state.config.claim_timeout_seconds);
    let mut last_digest = String::from("no status reported");
    loop {
        if let Some(sbx) = state.store.get_sandbox(name).await.map_err(map_store_err)? {
            if let Some(ip) = ready_pod_ip(&sbx) {
                tracing::info!(sandbox = %name, pod_ip = %ip, "sandbox resolved");
                return Ok(ResolvedSandbox {
                    name: name.to_string(),
                    pod_ip: ip,
                });
            }
            last_digest = condition_summary(sbx.status.as_ref());
        }
        if Instant::now() >= deadline {
            return Err(ApiError::ServiceUnavailable(format!(
                "sandbox {name} not ready in {}s ({last_digest})",
                state.config.claim_timeout_seconds
            )));
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// One-line digest of a sandbox status for error surfaces, e.g.
/// `phase=Pending; Ready=False reason=PodTerminating; PodScheduled=False
/// reason=Unschedulable: 0/1 nodes are available…`. Long condition messages
/// are clipped so the 503 stays one line.
fn condition_summary(status: Option<&SandboxStatus>) -> String {
    let Some(status) = status else {
        return "no status reported".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(phase) = &status.phase {
        parts.push(format!("phase={phase}"));
    }
    if let Some(conds) = &status.conditions {
        for c in conds {
            let mut s = format!("{}={}", c.r#type, c.status);
            if let Some(reason) = &c.reason {
                s.push_str(&format!(" reason={reason}"));
            }
            if let Some(msg) = &c.message {
                let clipped: String = msg.chars().take(120).collect();
                s.push_str(&format!(": {clipped}"));
            }
            parts.push(s);
        }
    }
    if parts.is_empty() {
        "status reported no phase or conditions".to_string()
    } else {
        parts.join("; ")
    }
}

/// Mirror the template pod's uid/group/fsGroup into the adoption Job so the
/// one-shot mover writes into the same-ownership PVC subPaths. Absent fields
/// (template without a pod securityContext) return None — the Job then runs
/// with the image defaults.
fn template_pod_ownership(pod_template: &serde_json::Value) -> crate::store::PodOwnership {
    let sc = pod_template.pointer("/spec/securityContext");
    let field = |name: &str| {
        sc.and_then(|s| s.get(name))
            .and_then(serde_json::Value::as_i64)
    };
    crate::store::PodOwnership {
        run_as_user: field("runAsUser"),
        run_as_group: field("runAsGroup"),
        fs_group: field("fsGroup"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SandboxStore as _;
    use shared::SandboxStatus;

    // --- template pod ownership mirroring (#182) ---------------------------

    #[test]
    fn template_pod_ownership_mirrors_and_omits() {
        let tpl = serde_json::json!({
            "spec": {"securityContext": {
                "runAsUser": 1000, "runAsGroup": 2000, "fsGroup": 3000
            }}
        });
        assert_eq!(
            template_pod_ownership(&tpl),
            crate::store::PodOwnership {
                run_as_user: Some(1000),
                run_as_group: Some(2000),
                fs_group: Some(3000),
            }
        );
        // Absent pod securityContext => all omitted (image defaults).
        assert_eq!(
            template_pod_ownership(&serde_json::json!({"spec": {}})),
            crate::store::PodOwnership::default()
        );
    }

    // --- name derivation (deterministic scheme) -----------------------------

    #[test]
    fn ephemeral_name_uses_pipe_separator_and_claim_prefix() {
        let name = sandbox_name("user-1", "chat-1", Profile::Ephemeral);
        assert!(name.starts_with(EPHEMERAL_PREFIX), "{name}");
        // Expected: sha256("user-1|chat-1").hexdigest()[:12]
        let want = hex12(b"user-1|chat-1");
        assert_eq!(name, format!("{EPHEMERAL_PREFIX}{want}"));
    }

    #[test]
    fn persistent_name_uses_slash_separator_and_chat_prefix() {
        let name = sandbox_name("user-1", "chat-1", Profile::Persistent);
        assert!(name.starts_with(CHAT_PREFIX), "{name}");
        let want = hex12(b"user-1/chat-1");
        assert_eq!(name, format!("{CHAT_PREFIX}{want}"));
    }

    #[test]
    fn names_are_deterministic_and_profile_dependent() {
        let e = sandbox_name("u", "s", Profile::Ephemeral);
        let p = sandbox_name("u", "s", Profile::Persistent);
        assert_ne!(e, p, "ephemeral and persistent must differ");
        // Same inputs ⇒ same name.
        assert_eq!(e, sandbox_name("u", "s", Profile::Ephemeral));
        assert_eq!(e, sandbox_name("u", "s", Profile::Ephemeral));
        assert_eq!(p, sandbox_name("u", "s", Profile::Persistent));
    }

    #[test]
    fn workspace_layout_per_user_pvc_is_user_pvc_plus_chat_subpath() {
        let config = shared::BrokerConfig::default(); // per-user-pvc
        let (claim, sub) = workspace_layout(&config, "user-1", "chat-1");
        let user = hex12(b"user-1");
        let chat = hex12(b"user-1/chat-1");
        assert_eq!(claim, format!("workspace-p-{user}"));
        assert_eq!(sub, format!("chats/{chat}"));
        // Deterministic + hash-only: raw ids never leak into the path.
        assert_eq!(sub, workspace_layout(&config, "user-1", "chat-1").1);
        assert!(!sub.contains("user-1") && !sub.contains("chat-1"));
    }

    #[test]
    fn workspace_layout_shared_subpath_namespaces_by_user() {
        let config = shared::BrokerConfig {
            persistent_mode: PersistentMode::SharedSubpath,
            shared_pvc_name: "workspace-shared".to_string(),
            ..Default::default()
        };
        let (claim, sub) = workspace_layout(&config, "user-1", "chat-1");
        let user = hex12(b"user-1");
        let chat = hex12(b"user-1/chat-1");
        assert_eq!(claim, "workspace-shared");
        assert_eq!(sub, format!("users/{user}/chats/{chat}"));
        // Two chats of one user share the PVC (and user dir) but NOT the subPath.
        let (_, sub2) = workspace_layout(&config, "user-1", "chat-2");
        assert_ne!(sub, sub2);
    }

    #[test]
    fn names_change_with_session() {
        assert_ne!(
            sandbox_name("u", "s1", Profile::Persistent),
            sandbox_name("u", "s2", Profile::Persistent)
        );
        assert_ne!(
            sandbox_name("u1", "s", Profile::Ephemeral),
            sandbox_name("u2", "s", Profile::Ephemeral)
        );
    }

    #[test]
    fn names_are_dns_label_safe_and_hex() {
        for profile in [Profile::Ephemeral, Profile::Persistent] {
            let name = sandbox_name("a@b/c.d", "é-1", profile);
            let body = match profile {
                Profile::Persistent => name.strip_prefix(CHAT_PREFIX).unwrap(),
                Profile::Ephemeral => name.strip_prefix(EPHEMERAL_PREFIX).unwrap(),
            };
            assert!(
                body.chars().all(|c| c.is_ascii_hexdigit()),
                "{name}: body must be lowercase hex"
            );
            assert_eq!(body.len(), 12, "{name}: 12 hex chars");
        }
    }

    // --- ready_pod_ip -------------------------------------------------------

    #[test]
    fn ready_pod_ip_returns_ip_when_ready_true() {
        let sbx = ready_fixture("10.0.0.5");
        assert_eq!(ready_pod_ip(&sbx).as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn ready_pod_ip_none_when_not_ready() {
        let sbx = not_ready_fixture();
        assert_eq!(ready_pod_ip(&sbx), None);
    }

    fn ready_fixture(ip: &str) -> Sandbox {
        let mut s = Sandbox::new("n", shared::SandboxSpec::default());
        s.status = Some(ready_status(ip));
        s
    }

    fn ready_status(ip: &str) -> SandboxStatus {
        SandboxStatus {
            phase: Some("Running".into()),
            pod_i_ps: Some(vec![ip.to_string()]),
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

    fn not_ready_fixture() -> Sandbox {
        let mut s = Sandbox::new("n", shared::SandboxSpec::default());
        s.status = Some(SandboxStatus {
            conditions: Some(vec![shared::SandboxCondition {
                r#type: "Ready".into(),
                status: "False".into(),
                reason: None,
                message: None,
                last_transition_time: None,
            }]),
            ..Default::default()
        });
        s
    }

    // --- not-ready 503 diagnostics (v0.5.6 mirrored conditions) ------------

    fn blocked_fixture() -> Sandbox {
        let mut s = Sandbox::new("n", shared::SandboxSpec::default());
        s.status = Some(SandboxStatus {
            phase: Some("Pending".into()),
            conditions: Some(vec![
                shared::SandboxCondition {
                    r#type: "Ready".into(),
                    status: "False".into(),
                    reason: Some("PodTerminating".into()),
                    message: None,
                    last_transition_time: None,
                },
                shared::SandboxCondition {
                    r#type: "PodScheduled".into(),
                    status: "False".into(),
                    reason: Some("Unschedulable".into()),
                    message: Some(
                        "0/1 nodes are available: 1 node(s) didn't match pod anti-affinity rules. "
                            .repeat(4),
                    ),
                    last_transition_time: None,
                },
            ]),
            ..Default::default()
        });
        s
    }

    #[test]
    fn condition_summary_includes_phase_reasons_and_clips_messages() {
        let s = blocked_fixture();
        let digest = condition_summary(s.status.as_ref());
        assert!(digest.contains("phase=Pending"), "{digest}");
        assert!(
            digest.contains("Ready=False reason=PodTerminating"),
            "{digest}"
        );
        assert!(
            digest.contains("PodScheduled=False reason=Unschedulable"),
            "{digest}"
        );
        // Long condition messages are clipped so the 503 stays one line.
        assert!(digest.len() < 400, "digest too long: {digest}");
    }

    #[test]
    fn condition_summary_handles_missing_status() {
        assert_eq!(condition_summary(None), "no status reported");
        let bare = SandboxStatus::default();
        assert_eq!(
            condition_summary(Some(&bare)),
            "status reported no phase or conditions"
        );
    }

    #[tokio::test]
    async fn not_ready_timeout_503_carries_condition_digest() {
        let config = shared::BrokerConfig {
            claim_timeout_seconds: 0,
            ..Default::default()
        };
        let state = AppState::for_test(config);
        let stub = crate::store::test_fakes::StubSandboxStore::new();
        stub.insert_sandbox(blocked_fixture());
        // for_test wires its own stub; rebuild the state with OUR stub so the
        // seeded sandbox is what the poll loop sees.
        let state = AppState {
            store: std::sync::Arc::new(stub),
            ..state
        };
        let result = wait_for_ready(&state, "n").await;
        let err = match result {
            Err(e) => e,
            Ok(r) => panic!("expected ServiceUnavailable, resolved: {r:?}"),
        };
        match err {
            ApiError::ServiceUnavailable(detail) => {
                assert!(detail.contains("not ready in 0s"), "{detail}");
                assert!(detail.contains("Unschedulable"), "{detail}");
                assert!(detail.contains("PodTerminating"), "{detail}");
            }
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    // --- #157 draft adoption -------------------------------------------------

    fn adoption_template() -> shared::SandboxTemplate {
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

    fn seeded_draft(user: &str, last_used: i64) -> Sandbox {
        let name = sandbox_name(user, user, Profile::Persistent);
        let mut s = Sandbox::new(name.as_str(), shared::SandboxSpec::default());
        let mut annots = std::collections::BTreeMap::new();
        annots.insert(
            crate::sandbox::LAST_USED_KEY.to_string(),
            last_used.to_string(),
        );
        s.metadata.annotations = Some(annots);
        s
    }

    async fn resolve_with_stub(
        config: shared::BrokerConfig,
        draft: Option<Sandbox>,
    ) -> (
        ResolvedSandbox,
        std::sync::Arc<crate::store::test_fakes::StubSandboxStore>,
    ) {
        let state = AppState::for_test(config);
        let stub = std::sync::Arc::new(crate::store::test_fakes::StubSandboxStore::new());
        stub.insert_template(adoption_template());
        stub.set_auto_ready_on_create(Some("10.42.0.9".to_string()));
        if let Some(d) = draft {
            stub.insert_sandbox(d);
        }
        let state = AppState {
            store: stub.clone(),
            ..state
        };
        let resolved = resolve_sandbox(&state, "user-1", "chat-9", Profile::Persistent)
            .await
            .expect("resolve");
        (resolved, stub)
    }

    /// #157 retry path: the first claim attempt timed out before readiness
    /// (slow first boot — e2e-verified live) and the in-memory plan died with
    /// it. The pending marker stamped at create time lets the SECOND resolve
    /// rebuild and run the move, then clears the marker (no re-run later).
    #[tokio::test]
    async fn adoption_survives_claim_retry_after_timeout() {
        // claim_timeout 0 → instant 503 on attempt 1 (never ready in time).
        let state = AppState::for_test(shared::BrokerConfig {
            claim_timeout_seconds: 0,
            ..shared::BrokerConfig::default()
        });
        let stub = std::sync::Arc::new(crate::store::test_fakes::StubSandboxStore::new());
        stub.insert_template(adoption_template());
        stub.insert_sandbox(seeded_draft("user-1", now_unix()));
        let state = AppState {
            store: stub.clone(),
            ..state
        };

        // Attempt 1: create + plan + stamp the marker, then time out (503).
        let err = resolve_sandbox(&state, "user-1", "chat-9", Profile::Persistent)
            .await
            .expect_err("first attempt must time out before readiness");
        assert!(matches!(err, ApiError::ServiceUnavailable(_)), "{err:?}");
        let chat_name = sandbox_name("user-1", "chat-9", Profile::Persistent);
        let chat = stub
            .get_sandbox(&chat_name)
            .await
            .expect("store ok")
            .expect("chat sandbox created");
        assert_eq!(
            chat.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(crate::sandbox::DRAFT_ADOPT_PENDING_KEY)),
            Some(&sandbox_name("user-1", "user-1", Profile::Persistent)),
            "pending marker stamped at create time"
        );
        assert!(stub.moves().is_empty(), "no move before readiness");

        // Attempt 2: the sandbox boots — the marker rebuilds the move.
        stub.set_sandbox_ready(&chat_name, "10.42.0.9");
        let resolved = resolve_sandbox(&state, "user-1", "chat-9", Profile::Persistent)
            .await
            .expect("retry resolve");
        assert_eq!(resolved.name, chat_name);
        let moves = stub.moves();
        assert_eq!(moves.len(), 1, "adoption ran on the retry");
        assert!(
            moves[0]
                .job_name
                .starts_with(&format!("draft-adopt-{chat_name}")),
            "job name carries the chat: {}",
            moves[0].job_name
        );

        // The one-shot marker is cleared — a later resolve never re-runs.
        let chat_after = stub
            .get_sandbox(&chat_name)
            .await
            .expect("store ok")
            .expect("chat sandbox");
        assert!(
            chat_after
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(crate::sandbox::DRAFT_ADOPT_PENDING_KEY))
                .is_none(),
            "marker cleared after the move ran"
        );
        resolve_sandbox(&state, "user-1", "chat-9", Profile::Persistent)
            .await
            .expect("third resolve");
        assert_eq!(stub.moves().len(), 1, "no re-run without the marker");
    }

    #[tokio::test]
    async fn fresh_chat_adopts_recent_draft_workspace() {
        let (resolved, stub) = resolve_with_stub(
            shared::BrokerConfig::default(),
            Some(seeded_draft("user-1", now_unix())),
        )
        .await;
        assert_eq!(
            resolved.name,
            sandbox_name("user-1", "chat-9", Profile::Persistent)
        );
        let moves = stub.moves();
        assert_eq!(moves.len(), 1, "exactly one adoption move");
        let mv = &moves[0];
        assert_eq!(mv.claim, format!("workspace-p-{}", hex12(b"user-1")));
        assert_eq!(
            mv.from_subpath,
            format!("chats/{}", hex12(b"user-1/user-1"))
        );
        assert_eq!(mv.to_subpath, format!("chats/{}", hex12(b"user-1/chat-9")));
        assert_eq!(mv.image, "code-standard:latest");
        assert!(
            mv.job_name.starts_with("draft-adopt-owui-c-"),
            "{}",
            mv.job_name
        );
    }

    #[tokio::test]
    async fn stale_draft_is_not_adopted() {
        let (_, stub) = resolve_with_stub(
            shared::BrokerConfig::default(),
            Some(seeded_draft("user-1", now_unix() - 100_000)),
        )
        .await;
        assert!(stub.moves().is_empty(), "stale draft must not move");
    }

    #[tokio::test]
    async fn draft_adoption_disabled_by_window_zero() {
        let config = shared::BrokerConfig {
            draft_adoption_window_seconds: 0,
            ..Default::default()
        };
        let (_, stub) = resolve_with_stub(config, Some(seeded_draft("user-1", now_unix()))).await;
        assert!(stub.moves().is_empty(), "window=0 disables adoption");
    }

    #[tokio::test]
    async fn no_draft_no_move() {
        let (_, stub) = resolve_with_stub(shared::BrokerConfig::default(), None).await;
        assert!(stub.moves().is_empty());
    }
}
