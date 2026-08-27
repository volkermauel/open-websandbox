//! S3-tiered cold storage (PR-C-4, issue #52): offload a sandbox's `/workspace`
//! to a bring-your-own S3-compatible bucket on reap, restore it on resume.
//!
//! The broker is the **sole S3 client** (#50 preserved): the runtime pod stays
//! network-isolated and only streams a `zstd` tarball of its workspace off
//! (`GET /snapshot`) and back on (`PUT /restore`). This module drives both
//! directions (D11):
//!
//! * **offload** ([`S3Offload`] implementing [`ReapOffload`]): `GET /snapshot`
//!   → `put_object` the new versioned key (SSE-S3) → keep-latest retention via
//!   **per-object `delete_object`** (NOT batch `DeleteObjects` — MinIO/R2/Proxmox
//!   reject the batch call with `MissingContentMD5`, #56), upload-new-then-
//!   delete-old ordering so the namespace is **never empty mid-offload** (D7/#56:
//!   a crash leaves the previous snapshot restorable). Transient failures retry
//!   with linear backoff and, on exhaustion, return [`OffloadError`] so the
//!   reaper **keeps the sandbox alive for the next tick** (no silent data loss).
//! * **restore** ([`S3Offload::restore_on_resume`]): list the namespace → newest
//!   object → `GET` it → `PUT /restore`. A no-op when there is no object (first
//!   creation); a failure surfaces as a 502 so the user never gets an empty
//!   workspace (D7).
//!
//! ## Testability
//!
//! [`ColdStore`] is a small `dyn`-safe trait; [`AwsColdStore`] is the real
//! `aws-sdk-s3` backend (D4) and [`test_fakes::InMemoryColdStore`] is a map-backed double,
//! so the offload/restore logic (key scheme, upload-then-delete ordering,
//! restore-skip-when-no-object, error→keep-alive) is exercised **without a live
//! S3 or cluster**. `wiremock` (dev-dep) stands in for the runtime's
//! `/snapshot` + `/restore` HTTP surface in the integration tests.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use kube::ResourceExt;

use shared::{BrokerConfig, Profile, Sandbox};

use crate::proxy::RUNTIME_PORT;
use crate::reaper::{OffloadError, ReapOffload};
use crate::sandbox::{PROFILE_LABEL_KEY, SESSION_KEY, USER_KEY};
use crate::store::SandboxStore;

/// Object-expiry/retention metadata + retry defaults
/// (`S3_RETENTION_DAYS=30`, `S3_OFFLOAD_MAX_ATTEMPTS=5`,
/// `S3_OFFLOAD_BACKOFF_SECONDS=10`). Exposed as constants so the wiring +
/// tests reference the documented defaults.
const DEFAULT_RETENTION_DAYS: u32 = 30;
const DEFAULT_OFFLOAD_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_OFFLOAD_BACKOFF: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Object-key scheme (D3): `<prefix>/<sandbox>/workspace-<ts>.tar.zst`
// ---------------------------------------------------------------------------

/// Per-session cold-tier namespace: `<prefix>/<user>/chats/<session>/`, keyed
/// by user + session for D11 parity — any broker reads objects another wrote,
/// and the e2e inspects them by user/chat. The
/// surrounding slashes of the configured prefix are stripped so the namespace is
/// canonical regardless of env formatting.
#[must_use]
pub fn s3_namespace(prefix: &str, user: &str, session: &str) -> String {
    let prefix = prefix.trim_matches('/');
    format!("{prefix}/{user}/chats/{session}/")
}

/// Versioned snapshot object key:
/// `<prefix>/<user>/chats/<session>/workspace-<ts>.tar.zst`. The timestamp is
/// zero-padded to 10 digits so **lexical order == chronological order** (D3) —
/// [`ColdStore::latest_key`] is therefore a lexical max.
#[must_use]
pub fn s3_object_key(prefix: &str, user: &str, session: &str, ts: i64) -> String {
    format!(
        "{}workspace-{ts:010}.tar.zst",
        s3_namespace(prefix, user, session)
    )
}

// ---------------------------------------------------------------------------
// ColdStore trait + real/in-memory backends
// ---------------------------------------------------------------------------

/// A failure from a cold-tier (S3) operation.
#[derive(Debug, thiserror::Error)]
pub enum ColdError {
    /// The S3 call failed (transport, auth, HTTP non-2xx, …).
    #[error("cold-tier (S3) error: {0}")]
    S3(String),
}

/// The cold-tier object store the broker offloads to / restores from, behind a
/// `dyn`-safe trait so unit tests use an in-memory double ([`test_fakes::InMemoryColdStore`])
/// instead of a live S3. [`AwsColdStore`] wraps `aws-sdk-s3` (decision D4).
#[async_trait]
pub trait ColdStore: Send + Sync {
    /// Upload `body` to `key` with SSE-S3 server-side encryption + the given
    /// retention metadata (R2/D5 object-expiry tagging).
    async fn put_object(
        &self,
        key: &str,
        body: Bytes,
        retention_days: u32,
    ) -> Result<(), ColdError>;

    /// Newest object key under `prefix` (lexical max == chronological), or
    /// `None` when the prefix is empty (first creation → restore is a no-op).
    async fn latest_key(&self, prefix: &str) -> Result<Option<String>, ColdError>;

    /// Fetch an object's body.
    async fn get_object(&self, key: &str) -> Result<Bytes, ColdError>;

    /// Delete every object under `prefix` except `skip` (keep-latest retention,
    /// R2/D5). Implemented as **per-object `delete_object`** — NOT batch
    /// `DeleteObjects` — so MinIO/R2/Proxmox (which reject the batch call with
    /// `MissingContentMD5`, #56) stay compatible. Returns the count removed.
    async fn delete_prefix_except(
        &self,
        prefix: &str,
        skip: Option<&str>,
    ) -> Result<u64, ColdError>;
}

/// Real cold-tier backend: an `aws-sdk-s3` client (decision D4).
///
/// Built once from [`BrokerConfig`] with path-style addressing
/// (MinIO/R2/Proxmox + works on AWS S3), an optional custom endpoint, and
/// static credentials when provided (else the SDK default chain applies).
pub struct AwsColdStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    /// Whether to request SSE-S3 (AES256) at rest. Disabled for stores without a
    /// KMS/SSE backend (dev `MinIO`) — driven by `BrokerConfig::s3_sse`.
    sse_enabled: bool,
}

impl AwsColdStore {
    /// Build the backend from the env-driven broker config (D12).
    #[must_use]
    pub fn new(cfg: &BrokerConfig) -> Self {
        use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};

        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.s3_region.clone()))
            .force_path_style(cfg.s3_path_style);
        if !cfg.s3_endpoint.is_empty() {
            builder = builder.endpoint_url(&cfg.s3_endpoint);
        }
        // Static credentials when provided; otherwise the SDK default chain
        // (env, IMDS, …) applies — bring-your-own credentials.
        if !cfg.s3_access_key_id.is_empty() {
            builder = builder.credentials_provider(Credentials::new(
                &cfg.s3_access_key_id,
                &cfg.s3_secret_access_key,
                None,
                None,
                "static",
            ));
        }
        Self {
            client: aws_sdk_s3::Client::from_conf(builder.build()),
            bucket: cfg.s3_bucket.clone(),
            // SSE-S3 only when the operator opted in (BROKER_S3_SSE non-empty / not
            // "none"); empty is required for dev MinIO (no SSE backend).
            sse_enabled: !cfg.s3_sse.is_empty() && !cfg.s3_sse.eq_ignore_ascii_case("none"),
        }
    }
}

#[async_trait]
impl ColdStore for AwsColdStore {
    async fn put_object(
        &self,
        key: &str,
        body: Bytes,
        retention_days: u32,
    ) -> Result<(), ColdError> {
        use aws_sdk_s3::types::ServerSideEncryption;
        // Object metadata (session-snapshot marker + retention-days, scanned on restore).
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("session-snapshot".to_string(), "1".to_string());
        metadata.insert("retention-days".to_string(), retention_days.to_string());
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .set_metadata(Some(metadata));
        // SSE-S3 (AES256) at rest (D9) — only when the operator opted in via
        // BROKER_S3_SSE (dev MinIO has no SSE backend and rejects the header).
        if self.sse_enabled {
            req = req.server_side_encryption(ServerSideEncryption::Aes256);
        }
        req.send().await.map_err(|e| ColdError::S3(e.to_string()))?;
        Ok(())
    }

    async fn latest_key(&self, prefix: &str) -> Result<Option<String>, ColdError> {
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .send()
            .await
            .map_err(|e| ColdError::S3(e.to_string()))?;
        // Lexical max == chronological (zero-padded ts keys, D3).
        Ok(resp
            .contents()
            .iter()
            .filter_map(|o| o.key().filter(|k| !k.is_empty()).map(str::to_owned))
            .max())
    }

    async fn get_object(&self, key: &str) -> Result<Bytes, ColdError> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| ColdError::S3(e.to_string()))?;
        resp.body
            .collect()
            .await
            .map_err(|e| ColdError::S3(e.to_string()))
            .map(aws_sdk_s3::primitives::AggregatedBytes::into_bytes)
    }

    async fn delete_prefix_except(
        &self,
        prefix: &str,
        skip: Option<&str>,
    ) -> Result<u64, ColdError> {
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .send()
            .await
            .map_err(|e| ColdError::S3(e.to_string()))?;
        let mut deleted = 0u64;
        for obj in resp.contents() {
            let Some(key) = obj.key().filter(|k| !k.is_empty()) else {
                continue;
            };
            if matches!(skip, Some(s) if s == key) {
                continue; // keep-latest: never delete the just-uploaded object.
            }
            // Per-object delete (NOT batch DeleteObjects): MinIO/R2/Proxmox
            // reject the batch call with MissingContentMD5 (#56).
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| ColdError::S3(e.to_string()))?;
            deleted += 1;
        }
        Ok(deleted)
    }
}

/// In-memory doubles for tests / local dev, re-exported so integration tests in
/// `tests/` can reuse them via `broker::test_fakes`.
pub mod test_fakes {
    use super::{ColdError, ColdStore};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// In-memory [`ColdStore`] double for tests and local dev (no live S3). Records
    /// the ordered call log so upload-then-delete ordering + keep-latest retention
    /// are asserted without a cluster.
    pub struct InMemoryColdStore {
        inner: Mutex<InMemoryInner>,
    }

    #[derive(Default)]
    struct InMemoryInner {
        objects: BTreeMap<String, Bytes>,
        /// Ordered operation log: `put:<key>` / `delete:<key>` / `get:<key>`.
        log: Vec<String>,
    }

    impl InMemoryColdStore {
        /// New empty store.
        #[must_use]
        pub fn new() -> Self {
            Self {
                inner: Mutex::new(InMemoryInner::default()),
            }
        }

        /// Seed a pre-existing object (e.g. to simulate a prior snapshot).
        pub fn seed(&self, key: &str, body: impl Into<Bytes>) {
            let mut g = self.inner.lock().expect("in-memory cold store");
            g.objects.insert(key.to_string(), body.into());
        }

        /// Ordered call log (for upload-then-delete ordering assertions).
        #[must_use]
        pub fn log(&self) -> Vec<String> {
            self.inner.lock().expect("in-memory cold store").log.clone()
        }

        /// Current object keys (sorted).
        #[must_use]
        pub fn keys(&self) -> Vec<String> {
            self.inner
                .lock()
                .expect("in-memory cold store")
                .objects
                .keys()
                .cloned()
                .collect()
        }
    }

    impl Default for InMemoryColdStore {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl ColdStore for InMemoryColdStore {
        async fn put_object(
            &self,
            key: &str,
            body: Bytes,
            _retention_days: u32,
        ) -> Result<(), ColdError> {
            let mut g = self.inner.lock().expect("in-memory cold store");
            g.log.push(format!("put:{key}"));
            g.objects.insert(key.to_string(), body);
            Ok(())
        }

        async fn latest_key(&self, prefix: &str) -> Result<Option<String>, ColdError> {
            let g = self.inner.lock().expect("in-memory cold store");
            Ok(g.objects
                .keys()
                .filter(|k| k.starts_with(prefix))
                .max()
                .cloned())
        }

        async fn get_object(&self, key: &str) -> Result<Bytes, ColdError> {
            let mut g = self.inner.lock().expect("in-memory cold store");
            g.log.push(format!("get:{key}"));
            g.objects
                .get(key)
                .cloned()
                .ok_or_else(|| ColdError::S3(format!("not found: {key}")))
        }

        async fn delete_prefix_except(
            &self,
            prefix: &str,
            skip: Option<&str>,
        ) -> Result<u64, ColdError> {
            let mut g = self.inner.lock().expect("in-memory cold store");
            let victims: Vec<String> = g
                .objects
                .keys()
                .filter(|k| k.starts_with(prefix))
                .filter(|k| skip != Some(k.as_str()))
                .cloned()
                .collect();
            let n = victims.len() as u64;
            for k in &victims {
                g.log.push(format!("delete:{k}"));
                g.objects.remove(k);
            }
            Ok(n)
        }
    }
}

// ---------------------------------------------------------------------------
// S3Offload — drives snapshot→S3 (reap) and S3→restore (resume)
// ---------------------------------------------------------------------------

/// Why a restore was deferred (surfaced to resolve so it can 502 / skip).
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// The restore failed (S3 read, transport, HTTP non-2xx). Resolve maps this
    /// to a 502 so the user never gets an empty workspace (D7).
    #[error("s3 restore failed: {0}")]
    Failed(String),
}

/// Outcome of a restore attempt (returns the restored key or `None`; the
/// `None` path is the first-creation no-op).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// An object was found and streamed to `/restore` (`Ok(key)` carried the
    /// restored key for logging/metrics).
    Restored(String),
    /// No object under the namespace — first creation; restore skipped.
    NoObject,
    /// The runtime's workspace was NON-empty (PVC hot hit — #142): the cold
    /// object exists but unpacking it over newer hot data would regress state,
    /// so the runtime skipped and keeps serving the hot tier.
    HotTierHit,
}

/// The S3-tiered offload/restore driver. Implements [`ReapOffload`] (the reaper
/// seam from PR-C-3) and exposes [`Self::restore_on_resume`] for the resolve
/// path. One instance is shared by the leader-gated reaper (offload) and the
/// request path (restore) when `broker.s3.enabled`.
pub struct S3Offload {
    cold: Arc<dyn ColdStore>,
    http: reqwest::Client,
    prefix: String,
    retention_days: u32,
    runtime_api_key: String,
    max_attempts: u32,
    backoff_base: Duration,
    /// Hot tier (#142): PVC-backed sandboxes get their chat dir PURGED from
    /// the hot tier after a fully-successful offload (true tiering: the PVC
    /// frees space); empty-dir sandboxes do not (the pod is deleted anyway).
    purge_hot_tier: bool,
    /// Bound for the resume-of-parked wait (pod must exist to snapshot).
    resume_timeout: Duration,
    /// Test/dev seam: when set, runtime hops target `{base}/snapshot` +
    /// `{base}/restore` instead of `http://<pod-ip>:8888/...` (used by the
    /// `wiremock`-backed integration tests). `None` in production.
    runtime_upstream_override: Option<String>,
    /// Per-session runtime-key resolver (PR-C-5); `None` in tests => shared-key fallback.
    store: Option<Arc<dyn SandboxStore>>,
}

impl S3Offload {
    /// Build the driver from the broker config + a concrete cold store + the
    /// shared proxy HTTP client. Retry policy + retention default to the documented
    /// values (`DEFAULT_OFFLOAD_MAX_ATTEMPTS` / `DEFAULT_OFFLOAD_BACKOFF` /
    /// `DEFAULT_RETENTION_DAYS`).
    #[must_use]
    pub fn new(cfg: &BrokerConfig, cold: Arc<dyn ColdStore>, http: reqwest::Client) -> Self {
        Self {
            cold,
            http,
            prefix: cfg.s3_prefix.clone(),
            retention_days: DEFAULT_RETENTION_DAYS,
            runtime_api_key: cfg.runtime_api_key.clone(),
            max_attempts: DEFAULT_OFFLOAD_MAX_ATTEMPTS,
            backoff_base: DEFAULT_OFFLOAD_BACKOFF,
            purge_hot_tier: cfg.persistent_mode.is_pvc(),
            resume_timeout: Duration::from_secs(cfg.claim_timeout_seconds.max(15)),
            runtime_upstream_override: None,
            store: None,
        }
    }

    /// Wire the live [`SandboxStore`] so offload/restore authenticate to the runtime
    /// with the per-session key (PR-C-5), matching the proxy path. Without it the
    /// driver falls back to the shared `BROKER_RUNTIME_API_KEY` (dev/tests only).
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn SandboxStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Resolve the runtime Bearer for `name`: the per-session key (PR-C-5) when a
    /// store is wired and the key exists, else the shared config key (dev/fallback).
    async fn runtime_key(&self, name: &str) -> String {
        if let Some(store) = &self.store {
            match store.read_runtime_key(name).await {
                Ok(Some(k)) => return k,
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(sandbox = %name, %e, "read_runtime_key failed; falling back to shared key");
                }
            }
        }
        self.runtime_api_key.clone()
    }

    /// Test seam: point runtime hops at a local mock server (e.g.
    /// `http://127.0.0.1:<port>` of a `wiremock` server).
    #[must_use]
    pub fn with_runtime_upstream_override(mut self, base: impl Into<String>) -> Self {
        self.runtime_upstream_override = Some(base.into());
        self
    }

    /// Test seam: shrink the retry policy so backoff loops don't sleep in tests.
    #[must_use]
    pub fn with_retry_policy(mut self, max_attempts: u32, backoff_base: Duration) -> Self {
        self.max_attempts = max_attempts;
        self.backoff_base = backoff_base;
        self
    }

    /// Runtime-hop URL for `path` (leading `/`): the override base when set,
    /// else `http://<pod-ip>:8888<path>` (hard-coded `:8888`).
    fn runtime_url(&self, pod_ip: &str, path: &str) -> String {
        match &self.runtime_upstream_override {
            Some(base) => format!("{base}{path}"),
            None => format!("http://{pod_ip}:{RUNTIME_PORT}{path}"),
        }
    }

    /// One offload attempt: snapshot → S3 put → keep-latest per-object delete.
    /// Upload-new-then-delete-old so the namespace is never empty mid-offload
    /// (D7/#56). Any failure bubbles up to the retry loop.
    async fn offload_once(
        &self,
        name: &str,
        pod_ip: &str,
        user: &str,
        session: &str,
    ) -> Result<(), OffloadError> {
        let ts = now_unix();
        let key = s3_object_key(&self.prefix, user, session, ts);
        let bearer = self.runtime_key(name).await;

        // GET /snapshot (the runtime streams the zstd tarball of /workspace).
        let snapshot = self
            .http
            .get(self.runtime_url(pod_ip, "/snapshot"))
            .bearer_auth(&bearer)
            .send()
            .await
            .map_err(|e| OffloadError::Failed(format!("snapshot GET {name}: {e}")))?;
        if !snapshot.status().is_success() {
            let st = snapshot.status();
            return Err(OffloadError::Failed(format!(
                "snapshot {name} -> HTTP {st}"
            )));
        }
        let body = snapshot
            .bytes()
            .await
            .map_err(|e| OffloadError::Failed(format!("snapshot read {name}: {e}")))?;

        // Upload NEW first (SSE-S3 at rest, D9).
        self.cold
            .put_object(&key, body, self.retention_days)
            .await
            .map_err(|e| OffloadError::Failed(format!("s3 put {key}: {e}")))?;

        // Then delete OLD under the namespace, skipping the just-uploaded key
        // (keep-latest; per-object delete; prefix never empty mid-offload).
        let ns = s3_namespace(&self.prefix, user, session);
        if let Err(e) = self.cold.delete_prefix_except(&ns, Some(&key)).await {
            // A keep-latest delete failure does NOT lose the snapshot (the new
            // object is already durably stored); surface it so the reaper can
            // decide, but the data is safe.
            return Err(OffloadError::Failed(format!(
                "s3 keep-latest delete {ns}: {e}"
            )));
        }
        tracing::info!(
            sandbox = %name, %key, user, session, "s3 offload complete"
        );
        Ok(())
    }

    /// Restore-on-resume: list the namespace →
    /// newest object → GET it → PUT `/restore`. A no-op ([`RestoreOutcome::NoObject`])
    /// when there is no object (first creation); any failure surfaces as
    /// [`RestoreError::Failed`] so resolve can fail the resume (502) rather than
    /// hand the user an empty workspace (D7).
    pub async fn restore_on_resume(
        &self,
        name: &str,
        pod_ip: &str,
        user: &str,
        session: &str,
    ) -> Result<RestoreOutcome, RestoreError> {
        let ns = s3_namespace(&self.prefix, user, session);
        let bearer = self.runtime_key(name).await;
        let Some(latest) = self
            .cold
            .latest_key(&ns)
            .await
            .map_err(|e| RestoreError::Failed(format!("s3 list {ns}: {e}")))?
        else {
            // Nothing to restore (first creation).
            return Ok(RestoreOutcome::NoObject);
        };
        let body = self
            .cold
            .get_object(&latest)
            .await
            .map_err(|e| RestoreError::Failed(format!("s3 get {latest}: {e}")))?;

        let resp = self
            .http
            .put(self.runtime_url(pod_ip, "/restore"))
            .bearer_auth(&bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/zstd")
            .body(body)
            .send()
            .await
            .map_err(|e| RestoreError::Failed(format!("restore PUT {name}: {e}")))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(RestoreError::Failed(format!(
                "restore {name} <- {latest} -> HTTP {st}: {}",
                text.chars().take(200).collect::<String>()
            )));
        }
        // Restore-if-empty (#142): a 200 with `restored: false` means the
        // runtime found a non-empty workspace (PVC hot hit — e.g. park resume)
        // and deliberately skipped so the hot tier keeps serving.
        let body = resp.text().await.unwrap_or_default();
        let restored = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("restored").and_then(serde_json::Value::as_bool))
            .unwrap_or(true);
        if !restored {
            tracing::info!(
                sandbox = %name, key = %latest,
                "s3 restore skipped — non-empty workspace (hot-tier hit)"
            );
            return Ok(RestoreOutcome::HotTierHit);
        }
        tracing::info!(sandbox = %name, key = %latest, "s3 restore complete");
        Ok(RestoreOutcome::Restored(latest))
    }
}

#[async_trait]
impl ReapOffload for S3Offload {
    async fn offload_on_reap(&self, sandbox: &Sandbox) -> Result<(), OffloadError> {
        // Cold tier offloads every persistent reap (#142): empty-dir hot
        // tier (its only durability) AND PVC hybrids (tiering — reap frees
        // the hot tier). Ephemeral sandboxes carry nothing to offload.
        if profile_of(sandbox) != Profile::Persistent {
            return Ok(());
        }
        let name = sandbox.name_any();
        let pod_ip = self.ensure_running_pod(sandbox).await?;
        let user = annotation(sandbox, USER_KEY);
        let session = annotation(sandbox, SESSION_KEY);

        // Retry with linear backoff (D7): on exhaustion, keep the sandbox alive.
        for attempt in 1..=self.max_attempts {
            match self.offload_once(&name, &pod_ip, &user, &session).await {
                Ok(()) => {
                    // #142: after a FULLY-successful offload (new object stored +
                    // keep-latest delete done), purge the chat dir from a PVC
                    // hot tier so it actually frees space. Best-effort: the
                    // object is durable, a stale dir merely wastes hot bytes
                    // until the next reap converges.
                    if self.purge_hot_tier {
                        self.purge_workspace(&name, &pod_ip).await;
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        %name, attempt, max = self.max_attempts, error = %e,
                        "s3 offload attempt failed"
                    );
                    if attempt < self.max_attempts {
                        tokio::time::sleep(self.backoff_base * attempt).await;
                    }
                }
            }
        }
        Err(OffloadError::Failed(format!(
            "offload {name} failed after {} attempts (keeping sandbox alive)",
            self.max_attempts
        )))
    }
}

impl S3Offload {
    /// Pod IP for the offload, resuming a parked sandbox first (#142).
    ///
    /// A `Suspended` (parked) PVC-hybrid sandbox has no pod, but the snapshot
    /// lives in the runtime — so the reaper briefly resumes it (patch
    /// `Running`, wait Ready, bounded by `claim_timeout_seconds`) before
    /// offloading. The sandbox is deleted right after, so the pod's lifetime
    /// is seconds. A `Running` sandbox without an IP yet keeps the old
    /// behaviour (error, leave for the next tick).
    async fn ensure_running_pod(&self, sandbox: &Sandbox) -> Result<String, OffloadError> {
        let name = sandbox.name_any();
        if let Some(ip) = sandbox.status.as_ref().and_then(|s| s.pod_ip()) {
            return Ok(ip.to_owned());
        }
        let suspended = sandbox
            .spec
            .operating_mode
            .is_some_and(|m| m == shared::OperatingMode::Suspended);
        if !suspended {
            return Err(OffloadError::Failed(format!(
                "no pod IP for {name}; cannot offload (leave for next tick)"
            )));
        }
        let Some(store) = self.store.as_ref() else {
            return Err(OffloadError::Failed(format!(
                "suspended {name} cannot be resumed without a store"
            )));
        };
        tracing::info!(sandbox = %name, "resuming parked sandbox for cold-tier offload");
        if let Err(e) = store
            .patch_operating_mode(&name, shared::OperatingMode::Running)
            .await
        {
            return Err(OffloadError::Failed(format!(
                "resume {name} for offload failed: {e} (leave for next tick)"
            )));
        }
        let deadline = tokio::time::Instant::now() + self.resume_timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(OffloadError::Failed(format!(
                    "resume {name} for offload timed out after {:?} (leave for next tick)",
                    self.resume_timeout
                )));
            }
            match store.get_sandbox(&name).await {
                Ok(Some(sbx)) => {
                    if let Some(ip) = sbx.status.as_ref().and_then(|s| s.pod_ip()) {
                        if sbx
                            .status
                            .as_ref()
                            .is_some_and(shared::SandboxStatus::is_ready)
                        {
                            return Ok(ip.to_owned());
                        }
                    }
                }
                Ok(None) => {
                    return Err(OffloadError::Failed(format!(
                        "sandbox {name} vanished while resuming for offload"
                    )));
                }
                Err(e) => {
                    tracing::warn!(sandbox = %name, %e, "poll during offload resume failed; retrying");
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Clear the workspace contents from the hot tier after a successful
    /// offload (#142). Contents only — never the subPath mount point itself —
    /// via the runtime's own `/execute` inside the sandbox.
    async fn purge_workspace(&self, name: &str, pod_ip: &str) {
        let bearer = self.runtime_key(name).await;
        let cmd = "find /workspace -mindepth 1 -delete";
        let payload = serde_json::json!({ "command": cmd });
        let res = self
            .http
            .post(self.runtime_url(pod_ip, "/execute"))
            .bearer_auth(&bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await;
        let outcome = match res {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(body)
                        if body.get("exit_code").and_then(serde_json::Value::as_i64) == Some(0) =>
                    {
                        Ok(())
                    }
                    Ok(body) => Err(format!("exited non-zero: {body}")),
                    Err(e) => Err(format!("response unreadable: {e}")),
                },
                Err(e) => Err(format!("response unreadable: {e}")),
            },
            Ok(resp) => Err(format!("HTTP {}", resp.status())),
            Err(e) => Err(format!("request failed: {e}")),
        };
        match outcome {
            Ok(()) => {
                tracing::info!(sandbox = %name, "purged chat dir from PVC hot tier after offload");
            }
            Err(why) => tracing::warn!(
                sandbox = %name, %why,
                "hot-tier purge failed (stale dir kept; S3 object is durable)"
            ),
        }
    }
}

/// Read the `broker-profile` label (defaults to ephemeral, matching the reaper).
fn profile_of(sbx: &Sandbox) -> Profile {
    match sbx
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(PROFILE_LABEL_KEY))
        .map(String::as_str)
    {
        Some("persistent") => Profile::Persistent,
        _ => Profile::Ephemeral,
    }
}

/// Read a sandbox annotation (empty string when absent).
fn annotation(sbx: &Sandbox, key: &str) -> String {
    sbx.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(key))
        .cloned()
        .unwrap_or_default()
}

/// Current epoch seconds (never panics on a pre-epoch clock).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::test_fakes::InMemoryColdStore;
    use super::*;
    use shared::{SandboxCondition, SandboxSpec, SandboxStatus};

    // --- object-key scheme (D3) -------------------------------------------

    #[test]
    fn namespace_is_prefix_user_chats_session() {
        assert_eq!(
            s3_namespace("users", "alice", "chat1"),
            "users/alice/chats/chat1/"
        );
        // Surrounding slashes on the prefix are canonicalised away.
        assert_eq!(
            s3_namespace("//prod/users//", "bob", "s2"),
            "prod/users/bob/chats/s2/"
        );
    }

    #[test]
    fn object_key_is_namespaced_versioned_and_zero_padded() {
        let key = s3_object_key("users", "alice", "chat1", 1_700_000_000);
        assert_eq!(key, "users/alice/chats/chat1/workspace-1700000000.tar.zst");
        // A small ts is still 10 digits so lexical order == chronological.
        let early = s3_object_key("users", "alice", "chat1", 5);
        assert_eq!(
            early, "users/alice/chats/chat1/workspace-0000000005.tar.zst",
            "zero-padded ts keeps lexical == chronological"
        );
    }

    #[test]
    fn lexical_order_of_keys_is_chronological() {
        let mut keys: Vec<String> = [5i64, 1_700_000_000, 999, 1_699_999_999]
            .iter()
            .map(|t| s3_object_key("users", "bob", "s2", *t))
            .collect();
        keys.sort();
        // Sorted ascending == chronological ascending.
        assert_eq!(
            keys.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "users/bob/chats/s2/workspace-0000000005.tar.zst",
                "users/bob/chats/s2/workspace-0000000999.tar.zst",
                "users/bob/chats/s2/workspace-1699999999.tar.zst",
                "users/bob/chats/s2/workspace-1700000000.tar.zst",
            ]
        );
    }

    // --- InMemoryColdStore keep-latest semantics --------------------------

    #[tokio::test]
    async fn latest_key_returns_lexical_max_under_prefix() {
        let store = InMemoryColdStore::new();
        let ns = s3_namespace("users", "sbx", "s1");
        store.seed(&format!("{ns}workspace-0000000005.tar.zst"), &b"old"[..]);
        store.seed(&format!("{ns}workspace-1700000000.tar.zst"), &b"new"[..]);
        store.seed("users/other-sbx/workspace-0000000001.tar.zst", &b"x"[..]);
        let latest = store.latest_key(&ns).await.unwrap();
        assert_eq!(
            latest.as_deref(),
            Some("users/sbx/chats/s1/workspace-1700000000.tar.zst"),
            "latest_key ignores other sandboxes + picks the newest ts"
        );
    }

    #[tokio::test]
    async fn delete_prefix_except_keeps_skip_and_removes_the_rest() {
        let store = InMemoryColdStore::new();
        let ns = s3_namespace("users", "sbx", "s1");
        let keep = format!("{ns}workspace-1700000000.tar.zst");
        let old1 = format!("{ns}workspace-1699000000.tar.zst");
        let old2 = format!("{ns}workspace-0000000005.tar.zst");
        let other = "users/other/workspace-1700000000.tar.zst".to_string();
        store.seed(&keep, &b"new"[..]);
        store.seed(&old1, &b"o1"[..]);
        store.seed(&old2, &b"o2"[..]);
        store.seed(&other, &b"x"[..]);

        let deleted = store.delete_prefix_except(&ns, Some(&keep)).await.unwrap();
        assert_eq!(
            deleted, 2,
            "only the two prior snapshots under the namespace"
        );
        let keys = store.keys();
        assert!(keys.contains(&keep), "the just-uploaded key is retained");
        assert!(!keys.contains(&old1));
        assert!(!keys.contains(&old2));
        assert!(
            keys.contains(&other),
            "objects outside the namespace are untouched"
        );
    }

    #[tokio::test]
    async fn get_object_returns_stored_body_or_errors() {
        let store = InMemoryColdStore::new();
        store.seed("k", &b"hello"[..]);
        assert_eq!(
            store.get_object("k").await.unwrap(),
            Bytes::from(&b"hello"[..])
        );
        assert!(store.get_object("missing").await.is_err());
    }

    // --- S3Offload: profile gate + restore skip + error→keep-alive ---------

    fn sandbox(profile: &str, pod_ip: Option<&str>) -> Sandbox {
        let mut sbx = Sandbox::new("n", SandboxSpec::default());
        let mut labels = std::collections::BTreeMap::new();
        labels.insert(PROFILE_LABEL_KEY.to_string(), profile.to_string());
        sbx.metadata.labels = Some(labels);
        let mut annots = std::collections::BTreeMap::new();
        annots.insert(USER_KEY.to_string(), "u".to_string());
        annots.insert(SESSION_KEY.to_string(), "s".to_string());
        sbx.metadata.annotations = Some(annots);
        if let Some(ip) = pod_ip {
            sbx.status = Some(SandboxStatus {
                phase: Some("Running".into()),
                pod_i_ps: Some(vec![ip.to_string()]),
                conditions: Some(vec![SandboxCondition {
                    r#type: "Ready".into(),
                    status: "True".into(),
                    reason: None,
                    message: None,
                    last_transition_time: None,
                }]),
                ready: Some(true),
                message: None,
            });
        }
        sbx
    }

    #[tokio::test]
    async fn offload_noops_for_ephemeral_profile() {
        // Ephemeral sandboxes carry nothing for the cold tier (only persistent
        // s3-tiered is offloaded). Must return Ok without touching S3.
        let store = Arc::new(InMemoryColdStore::new());
        let offload = S3Offload::new(
            &BrokerConfig::default(),
            store.clone(),
            reqwest::Client::new(),
        );
        let sbx = sandbox("ephemeral", Some("10.0.0.1"));
        offload.offload_on_reap(&sbx).await.unwrap();
        assert!(
            store.keys().is_empty(),
            "ephemeral reap wrote nothing to S3"
        );
    }

    #[tokio::test]
    async fn offload_errors_when_no_pod_ip_so_reaper_keeps_alive() {
        // A headless/parked sandbox has no pod IP → cannot snapshot → Err so
        // the reaper keeps the sandbox alive for the next tick (D7).
        let store = Arc::new(InMemoryColdStore::new());
        let offload = S3Offload::new(&BrokerConfig::default(), store, reqwest::Client::new());
        let sbx = sandbox("persistent", None);
        let err = offload.offload_on_reap(&sbx).await.unwrap_err();
        assert!(matches!(err, OffloadError::Failed(_)), "{err:?}");
    }

    // --- #142: resume-of-parked, hot-tier purge, restore-if-empty ----------

    fn wiremock_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("test client")
    }

    fn pvc_cfg() -> BrokerConfig {
        BrokerConfig {
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::PerUserPvc,
            ..Default::default()
        }
    }

    fn suspended_persistent(name: &str) -> Sandbox {
        let mut sbx = sandbox("persistent", None);
        sbx.metadata.name = Some(name.to_string());
        sbx.spec.operating_mode = Some(shared::OperatingMode::Suspended);
        sbx
    }

    #[tokio::test]
    async fn offload_resumes_suspended_sandbox_before_snapshot() {
        // A parked PVC-hybrid sandbox has no pod; the offloader must patch it
        // Running and wait for a Ready pod before snapshotting (#142).
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/snapshot"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"zstd-bytes"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/execute"))
            .and(body_partial_json(serde_json::json!({
                "command": "find /workspace -mindepth 1 -delete"
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "exit_code": 0, "stdout": "" })),
            )
            .expect(1..)
            .mount(&server)
            .await;

        let store = Arc::new(crate::store::test_fakes::StubSandboxStore::new());
        store.insert_sandbox(suspended_persistent("owui-c-res"));
        // Flip the sandbox Ready shortly after the resume patch lands, as the
        // upstream controller would.
        let flipper = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            flipper.mark_ready("owui-c-res", "10.1.2.3");
        });

        let offload = S3Offload::new(
            &pvc_cfg(),
            Arc::new(InMemoryColdStore::new()),
            wiremock_client(),
        )
        .with_store(store.clone())
        .with_runtime_upstream_override(server.uri());
        offload
            .offload_on_reap(&store.snapshot()["owui-c-res"].clone())
            .await
            .expect("offload after resume");

        // The resume actually happened through the store.
        assert_eq!(
            store.snapshot()["owui-c-res"].spec.operating_mode,
            Some(shared::OperatingMode::Running)
        );
    }

    #[tokio::test]
    async fn purge_is_skipped_for_empty_dir_hot_tier() {
        // empty-dir pods are deleted right after the reap; clearing them is
        // pointless — the purge hop must NOT fire.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/snapshot"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"zstd-bytes"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/execute"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let cfg = BrokerConfig {
            s3_enabled: true,
            persistent_mode: shared::PersistentMode::EmptyDir,
            ..Default::default()
        };
        let offload = S3Offload::new(&cfg, Arc::new(InMemoryColdStore::new()), wiremock_client())
            .with_runtime_upstream_override(server.uri());
        let sbx = sandbox("persistent", Some("10.0.0.9"));
        offload.offload_on_reap(&sbx).await.expect("offload ok");
    }

    #[tokio::test]
    async fn restore_hot_tier_hit_when_runtime_reports_non_empty() {
        // The runtime declines with `restored: false` (workspace-non-empty) —
        // the broker must surface HotTierHit, not an error.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/restore"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "restored": false, "bytes": 0, "skipped": "workspace-non-empty" }),
            ))
            .mount(&server)
            .await;

        let cold = Arc::new(InMemoryColdStore::new());
        cold.seed("users/u/chats/s/workspace-0000000001.tar.zst", &b"cold"[..]);
        let offload = S3Offload::new(&pvc_cfg(), cold, wiremock_client())
            .with_runtime_upstream_override(server.uri());
        let outcome = offload
            .restore_on_resume("owui-c-x", "10.0.0.1", "u", "s")
            .await
            .expect("skip is Ok");
        assert_eq!(outcome, RestoreOutcome::HotTierHit);
    }

    #[tokio::test]
    async fn restore_skips_when_no_object() {
        // First creation: no object under the namespace → NoObject, and the
        // runtime /restore hop is NEVER attempted (no HTTP server stood up).
        let store = Arc::new(InMemoryColdStore::new());
        let offload = S3Offload::new(&BrokerConfig::default(), store, reqwest::Client::new());
        let outcome = offload
            .restore_on_resume("owui-c-abc", "10.0.0.1", "alice", "chat1")
            .await
            .unwrap();
        assert_eq!(outcome, RestoreOutcome::NoObject);
    }
}
