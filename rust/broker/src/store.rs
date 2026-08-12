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

pub mod test_fakes {
    // In-memory doubles for tests / local dev. Kept under a clearly-named
    // `test_fakes` namespace so they do not clutter the production module
    // surface, but `pub` so integration tests in `tests/` can reuse them via
    // `broker::test_fakes`.
    use super::{SandboxStore, StoreError};
    use async_trait::async_trait;
    use kube::ResourceExt;
    use shared::{Sandbox, SandboxTemplate};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

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
    }

    impl StubSandboxStore {
        #[must_use]
        pub fn new() -> Self {
            Self {
                sandboxes: Mutex::new(HashMap::new()),
                templates: Mutex::new(HashMap::new()),
                reachable: AtomicBool::new(true),
                auto_ready_on_create: Mutex::new(None),
                runtime_keys: Mutex::new(HashMap::new()),
            }
        }

        /// Seed a template the store will return from [`get_template`](SandboxStore::get_template).
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
