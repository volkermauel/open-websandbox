//! Kubernetes CRD types for the open-websandbox control plane.
//!
//! Authored here as typed [`kube::CustomResource`] structs tracking the upstream
//! `agents.x-k8s.io/v1beta1` and `extensions.agents.x-k8s.io/v1beta1` groups
//! (D3). PR-A defines the two primary kinds — [`Sandbox`] and
//! [`SandboxTemplate`] — to prove the derive pipeline compiles against the
//! pinned dependency tree; the remaining kinds (`SandboxClaim`, `SandboxWarmPool`)
//! and the full field sets land in PR-C alongside the broker controllers.

#![forbid(unsafe_code)]

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Spec of a [`Sandbox`] — a running tenant sandbox managed by the broker.
#[derive(CustomResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1beta1",
    kind = "Sandbox",
    namespaced
)]
pub struct SandboxSpec {
    /// Name of the [`SandboxTemplate`] backing this sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,
}

/// Spec of a [`SandboxTemplate`] — the reusable blueprint a [`Sandbox`] instantiates.
#[derive(CustomResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[kube(
    group = "extensions.agents.x-k8s.io",
    version = "v1beta1",
    kind = "SandboxTemplate",
    namespaced
)]
pub struct SandboxTemplateSpec {
    /// Human-readable description of what this template provisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_serializes_with_correct_api_version_and_kind() {
        let sandbox = Sandbox::new(
            "demo",
            SandboxSpec {
                template_name: Some("base".into()),
            },
        );
        let value: serde_json::Value = serde_json::to_value(&sandbox).expect("serialize");
        assert_eq!(value["apiVersion"], "agents.x-k8s.io/v1beta1");
        assert_eq!(value["kind"], "Sandbox");
        assert_eq!(value["spec"]["templateName"], "base");
    }

    #[test]
    fn sandbox_round_trips_through_serde_json() {
        let sandbox = Sandbox::new(
            "demo",
            SandboxSpec {
                template_name: Some("base".into()),
            },
        );
        let json = serde_json::to_string(&sandbox).expect("serialize");
        let back: Sandbox = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.spec.template_name.as_deref(), Some("base"));
    }

    #[test]
    fn sandbox_template_uses_extension_group() {
        let tmpl = SandboxTemplate::new(
            "base",
            SandboxTemplateSpec {
                description: Some("d".into()),
            },
        );
        let value: serde_json::Value = serde_json::to_value(&tmpl).expect("serialize");
        assert_eq!(value["apiVersion"], "extensions.agents.x-k8s.io/v1beta1");
        assert_eq!(value["kind"], "SandboxTemplate");
    }
}
