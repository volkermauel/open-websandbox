//! Kubernetes Sandbox lifecycle backend, behind a stubbable trait.
//!
//! The HTTP handlers depend on [`SandboxStore`] (a `dyn`-safe trait), not on a
//! concrete kube client, so the request/response shaping, auth, and lifecycle
//! logic can be exercised in-process against an in-memory store without a live
//! cluster. [`KubeSandboxStore`] is the real backend: a typed
//! [`kube::Api<Sandbox>`] / [`kube::Api<SandboxTemplate>`] over the
//! `agents.x-k8s.io` / `extensions.agents.x-k8s.io` groups.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::Api;
use shared::{Sandbox, SandboxTemplate};

/// A failure from a Kubernetes lifecycle call, classified for HTTP mapping.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The named object does not exist (apiserver 404).
    #[error("not found")]
    NotFound,
    /// The named object already exists (apiserver 409).
    #[error("already exists")]
    Conflict,
    /// Any other apiserver / transport failure.
    #[error("kubernetes apiserver error: {0}")]
    Kube(#[source] kube::Error),
}

impl StoreError {
    /// Classify a raw [`kube::Error`] into a [`StoreError`].
    fn classify(err: kube::Error) -> Self {
        match &err {
            kube::Error::Api(resp) => match resp.code {
                404 => StoreError::NotFound,
                409 => StoreError::Conflict,
                _ => StoreError::Kube(err),
            },
            _ => StoreError::Kube(err),
        }
    }
}

/// The Kubernetes lifecycle operations the broker needs, as a `dyn`-safe trait.
///
/// Each method maps cleanly onto one kube-rs `Api` call so the real backend is a
/// thin I/O layer; the pure logic (building a `Sandbox` from a template,
/// computing readiness) lives in [`crate::sandbox`].
#[async_trait]
pub trait SandboxStore: Send + Sync {
    /// Fetch a `SandboxTemplate` by name (`Ok(None)` when absent).
    async fn get_template(&self, name: &str) -> Result<Option<SandboxTemplate>, StoreError>;

    /// Create a fully-formed `Sandbox`. Returns [`StoreError::Conflict`] when the
    /// name already exists; the caller decides whether to fetch-and-return the
    /// existing object.
    async fn create_sandbox(&self, sandbox: Sandbox) -> Result<Sandbox, StoreError>;

    /// Fetch a `Sandbox` by name (`Ok(None)` when absent).
    async fn get_sandbox(&self, name: &str) -> Result<Option<Sandbox>, StoreError>;

    /// Delete a `Sandbox` by name. Returns whether the object existed
    /// (404-tolerant).
    async fn delete_sandbox(&self, name: &str) -> Result<bool, StoreError>;

    /// List broker-owned `Sandbox` objects, optionally filtered by a Kubernetes
    /// label-selector expression.
    async fn list_sandboxes(
        &self,
        label_selector: Option<&str>,
    ) -> Result<Vec<Sandbox>, StoreError>;

    /// Park (`Suspended`) or resume (`Running`) a sandbox by patching
    /// `spec.operatingMode`.
    /// [`StoreError::NotFound`] when the object is absent.
    async fn patch_operating_mode(
        &self,
        name: &str,
        mode: shared::OperatingMode,
    ) -> Result<(), StoreError>;

    /// Refresh the `broker-last-used` annotation to `now` (epoch seconds) on the
    /// named sandbox. Active resolves call
    /// this so the reaper doesn't park/reap a sandbox mid-session. Best-effort
    /// at the call site: a failure is logged, never fatal. [`StoreError::NotFound`]
    /// when the object is absent.
    async fn touch_last_used(&self, name: &str, now: i64) -> Result<(), StoreError>;

    /// Remove one annotation key (merge-patch null; k8s preserves the rest).
    /// Backs the #157 one-shot draft-adoption marker. Best-effort at the call
    /// site: a failure is logged, never fatal.
    async fn clear_annotation(&self, name: &str, key: &str) -> Result<(), StoreError>;

    /// Is the apiserver reachable? Backs `GET /readyz` (503 when not).
    async fn apiserver_reachable(&self) -> bool;

    /// Get-or-create the per-session runtime API-key Secret
    /// (`owui-runtime-key-<sandbox>`) in the runtime namespace (PR-C-5 / #4).
    /// Idempotent — does NOT rotate an existing key (a stable key across a
    /// session's life is what the runtime caches + the broker re-sends each
    /// hop). Called before [`SandboxStore::create_sandbox`] so the non-optional
    /// runtime-key volume is satisfiable at pod-creation.
    async fn ensure_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError>;

    /// Stateless per-hop lookup of the per-session runtime API key. `Ok(None)`
    /// when the Secret is missing (misconfig / reaped session) so the hop goes
    /// out unauthenticated and the runtime fails closed (401/503). (PR-C-5 / #4)
    async fn read_runtime_key(&self, sandbox_name: &str) -> Result<Option<String>, StoreError>;

    /// Best-effort reap of the per-session key Secret with the sandbox
    /// (404-tolerant).
    async fn delete_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError>;

    /// Ensure the workspace PVC backing a persistent sandbox exists in the
    /// runtime namespace (#140).
    ///
    /// * `Some(spec)` (`per-user-pvc`): create-if-missing with the given
    ///   spec — a concurrent create (409) is tolerated.
    /// * `None` (`shared-subpath`): existence-check only. The chart renders
    ///   the shared PVC; a missing one is an install misconfiguration and
    ///   surfaces as [`StoreError::NotFound`] for the caller to turn into a
    ///   clear error.
    async fn ensure_workspace_pvc(
        &self,
        name: &str,
        create: Option<&WorkspacePvcSpec>,
    ) -> Result<(), StoreError>;

    /// Run a one-shot Job that moves `mv.from_subpath` → `mv.to_subpath` on
    /// `mv.claim` (draft adoption, #157). `Ok(true)` = move completed;
    /// `Ok(false)` = Job failed or timed out (adoption is best-effort — the
    /// caller logs and continues); `Err` = apiserver error.
    async fn move_workspace_dir(&self, mv: &WorkspaceMove) -> Result<bool, StoreError>;
}

/// Spec of the per-user workspace PVC the broker creates in `per-user-pvc`
/// mode (#140). All fields come straight from `BrokerConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePvcSpec {
    /// e.g. `["ReadWriteMany"]`.
    pub access_modes: Vec<String>,
    /// Kubernetes quantity string, e.g. `10Gi`.
    pub storage: String,
    /// StorageClass name; empty ⇒ cluster default.
    pub storage_class: String,
}

/// Pod uid/gid/fsGroup mirrored from the SandboxTemplate's pod
/// securityContext, so the one-shot adoption Job writes into the
/// same-ownership PVC subPaths (`None` => omit the field; the Job then
/// runs with the image defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PodOwnership {
    /// Pod `runAsUser` (None => omit; image default).
    pub run_as_user: Option<i64>,
    /// Pod `runAsGroup` (None => omit; image default).
    pub run_as_group: Option<i64>,
    /// Pod `fsGroup`, keeping group-write into the PVC (None => omit).
    pub fs_group: Option<i64>,
}

/// One-shot workspace move for draft adoption (#157): `from_subpath` →
/// `to_subpath` on `claim`, executed by a best-effort batch/v1 Job. Both
/// subpaths are broker-derived hash paths on the same claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMove {
    /// DNS-safe Job name (unique per adoption attempt).
    pub job_name: String,
    /// Job container image (the runtime image — guaranteed present).
    pub image: String,
    /// PVC both subpaths live on (per-user or shared).
    pub claim: String,
    /// Source directory (the draft workspace subPath).
    pub from_subpath: String,
    /// Destination directory (the chat workspace subPath).
    pub to_subpath: String,
    /// Seconds to wait for Job completion before giving up (best-effort).
    pub timeout_secs: u64,
    /// Pod uid/gid/fsGroup mirrored from the SandboxTemplate (see
    /// [`PodOwnership`]); the Job keeps writing into the sandbox pods'
    /// PVC subPaths.
    pub ownership: PodOwnership,
}

/// Real Kubernetes backend: typed [`Api`]s over a [`kube::Client`].
pub struct KubeSandboxStore {
    client: kube::Client,
    namespace: String,
}

impl KubeSandboxStore {
    /// Build a backend over `client` scoped to `namespace`.
    #[must_use]
    pub fn new(client: kube::Client, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }

    fn sandbox_api(&self) -> Api<Sandbox> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn template_api(&self) -> Api<SandboxTemplate> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }
}

#[async_trait]
impl SandboxStore for KubeSandboxStore {
    async fn get_template(&self, name: &str) -> Result<Option<SandboxTemplate>, StoreError> {
        match self.template_api().get(name).await {
            Ok(t) => Ok(Some(t)),
            Err(err) => match StoreError::classify(err) {
                StoreError::NotFound => Ok(None),
                other => Err(other),
            },
        }
    }

    async fn create_sandbox(&self, sandbox: Sandbox) -> Result<Sandbox, StoreError> {
        self.sandbox_api()
            .create(&PostParams::default(), &sandbox)
            .await
            .map_err(StoreError::classify)
    }

    async fn get_sandbox(&self, name: &str) -> Result<Option<Sandbox>, StoreError> {
        match self.sandbox_api().get(name).await {
            Ok(s) => Ok(Some(s)),
            Err(err) => match StoreError::classify(err) {
                StoreError::NotFound => Ok(None),
                other => Err(other),
            },
        }
    }

    async fn delete_sandbox(&self, name: &str) -> Result<bool, StoreError> {
        match self
            .sandbox_api()
            .delete(name, &DeleteParams::default())
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => match StoreError::classify(err) {
                StoreError::NotFound => Ok(false),
                other => Err(other),
            },
        }
    }

    async fn ensure_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError> {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = crate::runtime_key::secret_name(sandbox_name);
        match secrets.get(&name).await {
            Ok(_) => Ok(()), // exists — never rotate on the create/resolve path
            Err(kube::Error::Api(e)) if e.code == 404 => {
                let sec = crate::runtime_key::build_secret(
                    sandbox_name,
                    &self.namespace,
                    &crate::runtime_key::mint_key(),
                );
                match secrets.create(&PostParams::default(), &sec).await {
                    Ok(_) => Ok(()),
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(()), // concurrent ensure won
                    Err(err) => Err(StoreError::classify(err)),
                }
            }
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn read_runtime_key(&self, sandbox_name: &str) -> Result<Option<String>, StoreError> {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        match secrets
            .get(&crate::runtime_key::secret_name(sandbox_name))
            .await
        {
            Ok(sec) => Ok(sec
                .data
                .and_then(|d| d.get(crate::runtime_key::DATA_KEY).cloned())
                .and_then(|b| String::from_utf8(b.0).ok())),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn delete_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError> {
        use k8s_openapi::api::core::v1::Secret;
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        match secrets
            .delete(
                &crate::runtime_key::secret_name(sandbox_name),
                &DeleteParams::default(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => match StoreError::classify(err) {
                StoreError::NotFound => Ok(()), // best-effort / already gone
                other => Err(other),
            },
        }
    }

    async fn ensure_workspace_pvc(
        &self,
        name: &str,
        create: Option<&WorkspacePvcSpec>,
    ) -> Result<(), StoreError> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;

        let pvcs: Api<PersistentVolumeClaim> =
            Api::namespaced(self.client.clone(), &self.namespace);
        match pvcs.get(name).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => {
                let Some(spec) = create else {
                    // shared-subpath: the chart owns this PVC's lifecycle.
                    return Err(StoreError::NotFound);
                };
                let mut pvc_spec = serde_json::json!({
                    "accessModes": spec.access_modes,
                    "resources": {"requests": {"storage": spec.storage}},
                });
                if !spec.storage_class.is_empty() {
                    pvc_spec["storageClassName"] =
                        serde_json::Value::String(spec.storage_class.clone());
                }
                let pvc: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "PersistentVolumeClaim",
                    "metadata": {
                        "name": name,
                        "labels": {
                            "app.kubernetes.io/managed-by": "owui-broker",
                        },
                    },
                    "spec": pvc_spec,
                }))
                .map_err(|e| StoreError::Kube(kube::Error::SerdeError(e)))?;
                match pvcs.create(&PostParams::default(), &pvc).await {
                    Ok(_) => Ok(()),
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(()), // concurrent ensure won
                    Err(err) => Err(StoreError::classify(err)),
                }
            }
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn move_workspace_dir(&self, mv: &WorkspaceMove) -> Result<bool, StoreError> {
        use k8s_openapi::api::batch::v1::Job;

        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let job: Job = serde_json::from_value(adopt_job_spec(mv))
            .map_err(|e| StoreError::Kube(kube::Error::SerdeError(e)))?;

        match jobs.create(&PostParams::default(), &job).await {
            Ok(_) => {}
            // A previous adoption attempt left the Job behind (broker
            // restart mid-adoption): reuse it instead of failing.
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(err) => return Err(StoreError::classify(err)),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(mv.timeout_secs);
        loop {
            if let Ok(job) = jobs.get(&mv.job_name).await {
                let done = job
                    .status
                    .as_ref()
                    .and_then(|s| match (s.succeeded, s.failed) {
                        (Some(n), _) if n >= 1 => Some(true),
                        (_, Some(n)) if n >= 1 => Some(false),
                        _ => None,
                    });
                if let Some(ok) = done {
                    let _ = jobs.delete(&mv.job_name, &DeleteParams::default()).await;
                    return Ok(ok);
                }
            }
            if std::time::Instant::now() >= deadline {
                let _ = jobs.delete(&mv.job_name, &DeleteParams::default()).await;
                return Ok(false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn list_sandboxes(
        &self,
        label_selector: Option<&str>,
    ) -> Result<Vec<Sandbox>, StoreError> {
        let mut lp = ListParams::default();
        if let Some(sel) = label_selector {
            lp = lp.labels(sel);
        }
        self.sandbox_api()
            .list(&lp)
            .await
            .map(|list| list.items)
            .map_err(StoreError::classify)
    }

    async fn patch_operating_mode(
        &self,
        name: &str,
        mode: shared::OperatingMode,
    ) -> Result<(), StoreError> {
        // application/merge-patch+json over `spec.operatingMode` — the upstream
        // controller honours the Running/Suspended string literals.
        let patch = kube::api::Patch::Merge(serde_json::json!({
            "spec": { "operatingMode": mode }
        }));
        let params = kube::api::PatchParams::default();
        match self.sandbox_api().patch(name, &params, &patch).await {
            Ok(_) => Ok(()),
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn touch_last_used(&self, name: &str, now: i64) -> Result<(), StoreError> {
        // Merge-patch the single annotation key; k8s preserves the others.
        let patch = kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "annotations": { "broker-last-used": now.to_string() } }
        }));
        let params = kube::api::PatchParams::default();
        match self.sandbox_api().patch(name, &params, &patch).await {
            Ok(_) => Ok(()),
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn clear_annotation(&self, name: &str, key: &str) -> Result<(), StoreError> {
        // Merge-patch null removes exactly one key; k8s preserves the others.
        let patch = kube::api::Patch::Merge(serde_json::json!({
            "metadata": { "annotations": { key: null } }
        }));
        let params = kube::api::PatchParams::default();
        match self.sandbox_api().patch(name, &params, &patch).await {
            Ok(_) => Ok(()),
            Err(err) => Err(StoreError::classify(err)),
        }
    }

    async fn apiserver_reachable(&self) -> bool {
        // Lightweight readyz probe: list sandboxes
        // (limit 1). Any error → not ready.
        match self
            .sandbox_api()
            .list(&ListParams::default().limit(1))
            .await
        {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!("readyz: apiserver unreachable: {err}");
                false
            }
        }
    }
}

/// In-memory doubles for tests and local dev, re-exported so integration tests in
/// `tests/` can reuse them without a live apiserver.
pub mod test_fakes {
    use super::{SandboxStore, StoreError, WorkspacePvcSpec};
    use async_trait::async_trait;
    use kube::ResourceExt;
    use shared::{Sandbox, SandboxTemplate};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use super::WorkspaceMove;

    /// In-memory [`SandboxStore`] for tests and local dev (no apiserver required).
    ///
    /// Shipped in the library so integration tests can reuse it; it is a
    /// straightforward map-backed double, not production code.
    pub struct StubSandboxStore {
        sandboxes: Mutex<HashMap<String, Sandbox>>,
        templates: Mutex<HashMap<String, SandboxTemplate>>,
        reachable: AtomicBool,
        /// Pod IP stamped as a Ready status onto a sandbox at create time. `None`
        /// (default) ⇒ created sandboxes have no status, so a resolve poll will time
        /// out unless the test later calls [`Self::mark_ready`].
        auto_ready_on_create: Mutex<Option<String>>,
        /// Per-session runtime API keys (PR-C-5 / #4): keyed by sandbox name (the
        /// stub mimics the Secret the kube impl stores as
        /// `owui-runtime-key-<sandbox>`).
        runtime_keys: Mutex<HashMap<String, String>>,
        /// Workspace PVCs "created" via
        /// [`ensure_workspace_pvc`](SandboxStore::ensure_workspace_pvc), by name
        /// (#140).
        pvcs: Mutex<Vec<String>>,
        /// Recorded move_workspace_dir calls (draft adoption, #157), oldest
        /// first — lets tests assert the exact move the broker planned.
        job_moves: Mutex<Vec<WorkspaceMove>>,
    }

    impl StubSandboxStore {
        /// Construct an empty store with no sandboxes or templates.
        #[must_use]
        pub fn new() -> Self {
            Self {
                sandboxes: Mutex::new(HashMap::new()),
                templates: Mutex::new(HashMap::new()),
                reachable: AtomicBool::new(true),
                auto_ready_on_create: Mutex::new(None),
                runtime_keys: Mutex::new(HashMap::new()),
                pvcs: Mutex::new(Vec::new()),
                job_moves: Mutex::new(Vec::new()),
            }
        }

        /// Seed a template the store will return from [`get_template`](SandboxStore::get_template).
        #[must_use]
        pub fn with_template(self, template: SandboxTemplate) -> Self {
            self.insert_template(template);
            self
        }

        /// Insert (or replace) a template.
        pub fn insert_template(&self, template: SandboxTemplate) {
            let name = template.name_any();
            self.templates
                .lock()
                .expect("stub templates")
                .insert(name, template);
        }

        /// Test seam: seed a known per-session runtime key for `sandbox_name` so a
        /// test can assert the exact Bearer the proxy injects (otherwise
        /// `ensure_runtime_key` mints a random one). (PR-C-5 / #4)
        pub fn set_runtime_key(&self, sandbox_name: &str, key: &str) {
            self.runtime_keys
                .lock()
                .expect("stub runtime_keys")
                .insert(sandbox_name.to_string(), key.to_string());
        }

        /// Flip an EXISTING sandbox to Ready with a pod IP (simulates the
        /// controller finishing a slow boot after a first claim attempt timed
        /// out) — the retry-path counterpart of [`Self::set_auto_ready_on_create`].
        pub fn set_sandbox_ready(&self, name: &str, pod_ip: &str) {
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            if let Some(sbx) = map.get_mut(name) {
                sbx.status = Some(make_ready_status(pod_ip));
            }
        }

        /// Insert (or replace) a sandbox, e.g. to pre-seed a get/list scenario.
        pub fn insert_sandbox(&self, sandbox: Sandbox) {
            let name = sandbox.name_any();
            self.sandboxes
                .lock()
                .expect("stub sandboxes")
                .insert(name, sandbox);
        }

        /// Toggle whether [`apiserver_reachable`](SandboxStore::apiserver_reachable)
        /// reports healthy (default `true`).
        pub fn set_reachable(&self, reachable: bool) {
            self.reachable.store(reachable, Ordering::SeqCst);
        }

        /// Stamp a Ready status (with `pod_ip`) onto every sandbox created via
        /// [`create_sandbox`](SandboxStore::create_sandbox), simulating an instantly-
        /// ready controller. Pass `None` to leave created sandboxes status-less
        /// (e.g. to exercise the resolve timeout path).
        pub fn set_auto_ready_on_create(&self, pod_ip: Option<String>) {
            *self.auto_ready_on_create.lock().expect("stub auto_ready") = pod_ip;
        }

        /// Mark an existing sandbox Ready with `pod_ip` (flip the status the poll loop
        /// observes). Returns `false` if the sandbox is not present.
        pub fn mark_ready(&self, name: &str, pod_ip: &str) -> bool {
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            if let Some(sbx) = map.get_mut(name) {
                sbx.status = Some(make_ready_status(pod_ip));
                true
            } else {
                false
            }
        }

        /// Snapshot of the current sandbox store (name → `Sandbox`), for assertions.
        #[must_use]
        pub fn snapshot(&self) -> HashMap<String, Sandbox> {
            self.sandboxes.lock().expect("stub sandboxes").clone()
        }

        /// Recorded move_workspace_dir calls (draft adoption, #157).
        #[must_use]
        pub fn moves(&self) -> Vec<WorkspaceMove> {
            self.job_moves.lock().expect("stub job_moves").clone()
        }
    }

    impl Default for StubSandboxStore {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl SandboxStore for StubSandboxStore {
        async fn get_template(&self, name: &str) -> Result<Option<SandboxTemplate>, StoreError> {
            Ok(self
                .templates
                .lock()
                .expect("stub templates")
                .get(name)
                .cloned())
        }

        async fn create_sandbox(&self, mut sandbox: Sandbox) -> Result<Sandbox, StoreError> {
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            let name = sandbox.name_any();
            if map.contains_key(&name) {
                return Err(StoreError::Conflict);
            }
            // Simulate the controller flipping a freshly-created sandbox to Ready.
            if let Some(ip) = self
                .auto_ready_on_create
                .lock()
                .expect("stub auto_ready")
                .clone()
            {
                sandbox.status = Some(make_ready_status(&ip));
            }
            map.insert(name.clone(), sandbox.clone());
            Ok(sandbox)
        }

        async fn get_sandbox(&self, name: &str) -> Result<Option<Sandbox>, StoreError> {
            Ok(self
                .sandboxes
                .lock()
                .expect("stub sandboxes")
                .get(name)
                .cloned())
        }

        async fn delete_sandbox(&self, name: &str) -> Result<bool, StoreError> {
            Ok(self
                .sandboxes
                .lock()
                .expect("stub sandboxes")
                .remove(name)
                .is_some())
        }

        async fn list_sandboxes(
            &self,
            label_selector: Option<&str>,
        ) -> Result<Vec<Sandbox>, StoreError> {
            let items: Vec<Sandbox> = self
                .sandboxes
                .lock()
                .expect("stub sandboxes")
                .values()
                .cloned()
                .collect();
            Ok(match label_selector {
                // Support a single `key=value` selector (the subset tests/exercises use).
                Some(sel) => {
                    let want = sel.split('=').collect::<Vec<_>>();
                    if want.len() == 2 {
                        items
                            .into_iter()
                            .filter(|s| {
                                s.metadata
                                    .labels
                                    .as_ref()
                                    .and_then(|l| l.get(want[0]))
                                    .is_some_and(|v| v == want[1])
                            })
                            .collect()
                    } else {
                        items
                    }
                }
                None => items,
            })
        }

        async fn apiserver_reachable(&self) -> bool {
            self.reachable.load(Ordering::SeqCst)
        }

        async fn patch_operating_mode(
            &self,
            name: &str,
            mode: shared::OperatingMode,
        ) -> Result<(), StoreError> {
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            match map.get_mut(name) {
                Some(sbx) => {
                    sbx.spec.operating_mode = Some(mode);
                    Ok(())
                }
                None => Err(StoreError::NotFound),
            }
        }

        async fn touch_last_used(&self, name: &str, now: i64) -> Result<(), StoreError> {
            use crate::sandbox::LAST_USED_KEY;
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            match map.get_mut(name) {
                Some(sbx) => {
                    let annots = sbx.metadata.annotations.get_or_insert_with(BTreeMap::new);
                    annots.insert(LAST_USED_KEY.to_string(), now.to_string());
                    Ok(())
                }
                None => Err(StoreError::NotFound),
            }
        }

        async fn clear_annotation(&self, name: &str, key: &str) -> Result<(), StoreError> {
            let mut map = self.sandboxes.lock().expect("stub sandboxes");
            match map.get_mut(name) {
                Some(sbx) => {
                    if let Some(annots) = sbx.metadata.annotations.as_mut() {
                        annots.remove(key);
                    }
                    Ok(())
                }
                None => Err(StoreError::NotFound),
            }
        }

        async fn move_workspace_dir(&self, mv: &WorkspaceMove) -> Result<bool, StoreError> {
            self.job_moves
                .lock()
                .expect("stub job_moves")
                .push(mv.clone());
            Ok(true)
        }
        async fn ensure_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError> {
            let mut keys = self.runtime_keys.lock().expect("stub runtime_keys");
            keys.entry(sandbox_name.to_string())
                .or_insert_with(crate::runtime_key::mint_key);
            Ok(())
        }

        async fn read_runtime_key(&self, sandbox_name: &str) -> Result<Option<String>, StoreError> {
            Ok(self
                .runtime_keys
                .lock()
                .expect("stub runtime_keys")
                .get(sandbox_name)
                .cloned())
        }

        async fn delete_runtime_key(&self, sandbox_name: &str) -> Result<(), StoreError> {
            self.runtime_keys
                .lock()
                .expect("stub runtime_keys")
                .remove(sandbox_name);
            Ok(())
        }

        async fn ensure_workspace_pvc(
            &self,
            name: &str,
            create: Option<&WorkspacePvcSpec>,
        ) -> Result<(), StoreError> {
            let mut pvcs = self.pvcs.lock().expect("stub pvcs");
            if pvcs.iter().any(|n| n == name) {
                return Ok(());
            }
            let Some(spec) = create else {
                return Err(StoreError::NotFound); // shared PVC must be pre-seeded
            };
            let _ = spec; // recorded by name; spec fields asserted via the kube impl's tests
            pvcs.push(name.to_string());
            Ok(())
        }
    }

    /// Build a Ready `SandboxStatus` for the stub (a controller would populate this
    /// as it scheduled the pod).
    fn make_ready_status(pod_ip: &str) -> shared::SandboxStatus {
        use shared::{SandboxCondition, SandboxStatus};
        SandboxStatus {
            phase: Some("Running".to_string()),
            pod_i_ps: Some(vec![pod_ip.to_string()]),
            conditions: Some(vec![SandboxCondition {
                r#type: "Ready".to_string(),
                status: "True".to_string(),
                reason: None,
                message: None,
                last_transition_time: None,
            }]),
            ready: Some(true),
            message: None,
        }
    }
}

/// Build the one-shot draft-adoption Job manifest (#157).
///
/// PodSecurity `restricted:latest` compliant — the runtime namespace may
/// enforce it, and the previous manifest (no securityContext at all) was
/// Forbidden on every attempt: the Job never started, the broker logged
/// "draft adoption failed" on each resolve, and the deadline expired into
/// DeadlineExceeded. The pod now pins `runAsNonRoot` + `seccompProfile`
/// and mirrors the template's `runAsUser`/`runAsGroup`/`fsGroup` so the
/// mover keeps writing into the sandbox pods' PVC subPaths; the container
/// drops all capabilities without privilege escalation. The Job touches no
/// Kubernetes API, so its service-account token is not mounted.
fn adopt_job_spec(mv: &WorkspaceMove) -> serde_json::Value {
    let mut pod_sc = serde_json::json!({
        "seccompProfile": {"type": "RuntimeDefault"},
    });
    // A mirrored uid of 0 means the Job runs as root: pairing it with
    // `runAsNonRoot` is unsatisfiable and the pod could never start, even
    // on unrestricted sites — so the pin only applies to non-root uids.
    if mv.ownership.run_as_user != Some(0) {
        pod_sc["runAsNonRoot"] = serde_json::Value::Bool(true);
    }
    for (key, val) in [
        ("runAsUser", mv.ownership.run_as_user),
        ("runAsGroup", mv.ownership.run_as_group),
        ("fsGroup", mv.ownership.fs_group),
    ] {
        if let Some(v) = val {
            pod_sc[key] = v.into();
        }
    }
    serde_json::json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": mv.job_name,
            "labels": {
                "app.kubernetes.io/managed-by": "owui-broker",
                "broker-component": "draft-adoption",
            },
        },
        "spec": {
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 300,
            "activeDeadlineSeconds": mv.timeout_secs,
            "template": {
                "spec": {
                    "restartPolicy": "Never",
                    "automountServiceAccountToken": false,
                    "securityContext": pod_sc,
                    "containers": [{
                        "name": "adopt",
                        "image": mv.image,
                        // Subpaths are broker-derived hash paths (no
                        // injection surface), but they still travel via
                        // env — the shell never interpolates them.
                        "command": ["/bin/sh", "-ec",
                            "mkdir -p \"$CHAT_DIR\"; find \"$DRAFT_DIR\" -mindepth 1 -maxdepth 1 ! -name '.open-websandbox' -exec mv {} \"$CHAT_DIR/\" \\; ; rmdir \"$DRAFT_DIR\" 2>/dev/null || true"],
                        "env": [
                            {"name": "DRAFT_DIR", "value": format!("/pvc/{}", mv.from_subpath)},
                            {"name": "CHAT_DIR", "value": format!("/pvc/{}", mv.to_subpath)},
                        ],
                        "volumeMounts": [{"name": "workspace", "mountPath": "/pvc"}],
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "capabilities": {"drop": ["ALL"]},
                        },
                    }],
                    "volumes": [{"name": "workspace",
                        "persistentVolumeClaim": {"claimName": mv.claim}}],
                },
            },
        },
    })
}

#[cfg(test)]
mod adopt_job_spec_tests {
    use super::*;

    fn mv() -> WorkspaceMove {
        WorkspaceMove {
            job_name: "draft-adopt-owui-c-test-1".into(),
            image: "runtime:latest".into(),
            claim: "workspace-p-test".into(),
            from_subpath: "chats/aa".into(),
            to_subpath: "chats/bb".into(),
            timeout_secs: 60,
            ownership: PodOwnership {
                run_as_user: Some(1000),
                run_as_group: Some(1000),
                fs_group: Some(1000),
            },
        }
    }

    /// PodSecurity `restricted:latest` admission: every field the profile
    /// mandates must be present, or the Job is Forbidden (the FailedCreate
    /// loop observed on enforcing clusters).
    #[test]
    fn job_is_restricted_compliant() {
        let j = adopt_job_spec(&mv());
        let pod = &j["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            pod["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        let c = &pod["containers"][0]["securityContext"];
        assert_eq!(c["allowPrivilegeEscalation"], false);
        assert_eq!(c["capabilities"]["drop"][0], "ALL");
    }

    #[test]
    fn mirrors_template_ownership_and_omits_when_absent() {
        let j = adopt_job_spec(&mv());
        let sc = &j["spec"]["template"]["spec"]["securityContext"];
        assert_eq!(sc["runAsUser"], 1000);
        assert_eq!(sc["runAsGroup"], 1000);
        assert_eq!(sc["fsGroup"], 1000);

        let mut m = mv();
        m.ownership = PodOwnership::default();
        let sc = &adopt_job_spec(&m)["spec"]["template"]["spec"]["securityContext"];
        assert!(sc.get("runAsUser").is_none());
        assert!(sc.get("runAsGroup").is_none());
        assert!(sc.get("fsGroup").is_none());
        // Restricted-compliance fields survive the Option-less path.
        assert_eq!(sc["runAsNonRoot"], true);
    }

    /// A template that (against the chart default) runs as uid 0 must not
    /// get `runAsNonRoot: true` — the pair is unsatisfiable and the Job
    /// could never start, even on unrestricted sites.
    #[test]
    fn root_uid_skips_run_as_non_root() {
        let mut m = mv();
        m.ownership.run_as_user = Some(0);
        let sc = &adopt_job_spec(&m)["spec"]["template"]["spec"]["securityContext"];
        assert_eq!(sc["runAsUser"], 0);
        assert!(sc.get("runAsNonRoot").is_none());
    }
}
