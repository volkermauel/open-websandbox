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
        PersistentMode::PerUserPvc | PersistentMode::S3Tiered => (
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
                Err(e) => {
                    return Err(ApiError::BadGateway(format!("s3 restore failed: {e}")));
                }
            }
        }
    }
    Ok(resolved)
}

/// Poll the store until the named Sandbox is Ready with a pod IP, or the deadline.
async fn wait_for_ready(state: &AppState, name: &str) -> Result<ResolvedSandbox, ApiError> {
    let deadline = Instant::now() + Duration::from_secs(state.config.claim_timeout_seconds);
    loop {
        if let Some(sbx) = state.store.get_sandbox(name).await.map_err(map_store_err)? {
            if let Some(ip) = ready_pod_ip(&sbx) {
                tracing::info!(sandbox = %name, pod_ip = %ip, "sandbox resolved");
                return Ok(ResolvedSandbox {
                    name: name.to_string(),
                    pod_ip: ip,
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(ApiError::ServiceUnavailable(format!(
                "sandbox {name} not ready in {}s",
                state.config.claim_timeout_seconds
            )));
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::SandboxStatus;

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
}
