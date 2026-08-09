//! Kubernetes client construction.
//!
//! Builds a [`kube::Client`] from in-cluster service-account config, falling
//! back to the local kubeconfig for dev/tests — the same try/fallback the
//! Python broker performs with `config.load_incluster_config()` /
//! `config.load_kube_config()`. [`kube::Config::infer`] does both in one call
//! (in-cluster first, kubeconfig second), which is exactly the desired order.

#![forbid(unsafe_code)]

/// Build a [`kube::Client`] from the environment: in-cluster config when running
/// as a pod, the local kubeconfig otherwise.
///
/// Errors (no service-account mounted AND no kubeconfig on disk) propagate to
/// the binary, which refuses to boot — the broker cannot serve without an
/// apiserver.
pub async fn build_client() -> anyhow::Result<kube::Client> {
    let config = kube::Config::infer().await?;
    Ok(kube::Client::try_from(config)?)
}
