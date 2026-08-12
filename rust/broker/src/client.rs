//! Kubernetes client construction.
//!
//! Builds a [`kube::Client`] from in-cluster service-account config, falling
//! back to the local kubeconfig for dev/tests. [`kube::Config::infer`] does
//! both in one call (in-cluster first, kubeconfig second), which is exactly the
//! desired order.

#![forbid(unsafe_code)]

/// Typed failure of [`build_client`].
///
/// Replaces the former `anyhow::Result` so a kube/config/connection failure
/// surfaces as a structured error (D2) rather than an opaque `anyhow` blob the
/// binary can only format. Propagates to `main`, which logs a precise cause and
/// refuses to boot — the broker cannot serve without an apiserver.
#[derive(Debug, thiserror::Error)]
pub enum ClientBuildError {
    /// `kube::Config::infer` could find neither an in-cluster service account
    /// nor a local kubeconfig.
    #[error("failed to infer Kubernetes config (no in-cluster service account and no local kubeconfig): {0}")]
    Infer(#[from] kube::config::InferConfigError),

    /// The inferred config could not be turned into a `kube::Client`.
    #[error("failed to construct Kubernetes client: {0}")]
    Construct(#[from] kube::Error),
}

/// Build a [`kube::Client`] from the environment: in-cluster config when running
/// as a pod, the local kubeconfig otherwise.
pub async fn build_client() -> Result<kube::Client, ClientBuildError> {
    let config = kube::Config::infer().await?;
    Ok(kube::Client::try_from(config)?)
}
