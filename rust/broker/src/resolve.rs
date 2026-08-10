//! Deterministic session→sandbox resolution + ready-polling.
//!
//! Mirrors the Python broker's `resolve_sandbox` / `_ephemeral_sandbox_name` /
//! `_chat_sandbox_name` (PR-C-2): compute the deterministic per-session Sandbox
//! name, get-or-create it via the [`SandboxStore`], then poll the store until the
//! upstream controller reports `Ready` **and** a pod IP (reusing
//! [`SandboxStatus::is_ready`](shared::SandboxStatus::is_ready) +
//! [`pod_ip`](shared::SandboxStatus::pod_ip)).
//!
//! Out of scope here (land in later PRs, matching the Python order): per-session
//! runtime-key mint/rotate (C-3), staging→chat migration (C-3), S3-tiered restore
//! (C-4). The Python broker resumes a `Suspended` sandbox before watching; the
//! Rust controller-set interaction (park/resume) is C-3/leader-election territory,
//! so a present-but-not-Ready sandbox is simply polled here.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use shared::{Profile, Sandbox, SandboxStatus};

use crate::error::ApiError;
use crate::sandbox::{build_sandbox, extract_pod_template};
use crate::state::AppState;
use crate::store::StoreError;

/// Prefix for ephemeral (per-SESSION, emptyDir) sandboxes (Python `CLAIM_PREFIX`).
pub const EPHEMERAL_PREFIX: &str = "owui-";
/// Prefix for persistent (per-CHAT) sandboxes (Python `CHAT_PREFIX`).
pub const CHAT_PREFIX: &str = "owui-c-";

/// Poll interval while waiting for a Sandbox to reach `Ready`. The Python broker
/// uses a server-side Watch; PR-C-2 polls the store (simpler, fine for C-2 per
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
/// * Ephemeral profile: `owui-` + `sha256("{user}|{session}")[:12]`
///   (Python `_ephemeral_sandbox_name`).
/// * Persistent profile: `owui-c-` + `sha256("{user}/{session}")[:12]`
///   (Python `_chat_sandbox_name`).
///
/// Byte-identical to the Python scheme — including the differing separator (`|`
/// vs `/`) and prefix (`owui-` vs `owui-c-`) — so a Rust broker resolves the SAME
/// object a Python broker created for a given user/session (D11 cutover safety).
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

/// First 12 hex chars (6 bytes) of `sha256(input)` — matches Python `hexdigest()[:12]`.
fn hex12(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let out = hasher.finalize();
    out.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Pod IP of a `Ready` sandbox, or `None` (Python `_sandbox_ready_with_ip`).
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
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the per-session Sandbox: get-or-create, then poll until Ready + IP.
///
/// 1. Compute the deterministic [`sandbox_name`].
/// 2. [`SandboxStore::get_sandbox`]; if absent, clone the base template's
///    `podTemplate` via [`build_sandbox`] and [`SandboxStore::create_sandbox`].
///    A `Conflict` means a concurrent create won — fall through to the poll.
/// 3. Poll [`SandboxStore::get_sandbox`] every [`READY_POLL_INTERVAL`] until
///    [`ready_pod_ip`] or the configured `claim_timeout_seconds` deadline
///    (→ 503, matching the PR-C-2 spec; the Python broker raises 504).
///
/// Returns the ready [`ResolvedSandbox`] (name + pod IP).
pub async fn resolve_sandbox(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    profile: Profile,
) -> Result<ResolvedSandbox, ApiError> {
    let name = sandbox_name(user_id, session_id, profile);

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
        let pod_template = extract_pod_template(&template)?;

        // PR-C-5 / #4: ensure the per-session runtime-key Secret exists BEFORE
        // the Sandbox is created, so the non-optional runtime-key volume is
        // satisfiable when the controller schedules the pod. Fail-fast: a missing
        // key would CrashLoop the runtime (fail-closed boot guard).
        state.store.ensure_runtime_key(&name).await.map_err(map_store_err)?;
        let sandbox = build_sandbox(
            &name,
            Some(user_id),
            Some(session_id),
            profile,
            pod_template,
            &state.config.runtime_ns,
            now_unix(),
        );
        match state.store.create_sandbox(sandbox).await {
            Ok(_) => {}
            // A concurrent resolve won the create race — poll for the winner.
            Err(StoreError::Conflict) => {}
            Err(e) => return Err(map_store_err(e)),
        }
    }
    // Resume a parked sandbox (Python `_resume_if_suspended` on every watch
    // event): flip a `Suspended` sandbox back to `Running` so its pod schedules
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
    // Activity bump (Python `_touch_sandbox`): refresh `broker-last-used` so the
    // leader's reaper doesn't park/reap a sandbox mid-session. Best-effort — the
    // resolve already succeeded, so a patch failure is logged, never fatal
    // (mirrors the Python `except ... log.debug` swallow).
    if let Err(e) = state
        .store
        .touch_last_used(&resolved.name, now_unix())
        .await
    {
        tracing::debug!(sandbox = %resolved.name, error = %e, "non-fatal last-used touch");
    }
    // C-4 restore-on-resume (Python `_restore_from_s3`, gated on S3 tiering +
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

    // --- name derivation (parity with the Python scheme) --------------------

    #[test]
    fn ephemeral_name_uses_pipe_separator_and_claim_prefix() {
        let name = sandbox_name("user-1", "chat-1", Profile::Ephemeral);
        assert!(name.starts_with(EPHEMERAL_PREFIX), "{name}");
        // Python: sha256("user-1|chat-1").hexdigest()[:12]
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
        assert_eq!(p, sandbox_name("u", "s", Profile::Persistent));
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
