//! Kubernetes CRD types for the open-websandbox control plane.
//!
//! Authored here as typed [`kube::CustomResource`] structs tracking the upstream
//! `agents.x-k8s.io/v1beta1` and `extensions.agents.x-k8s.io/v1beta1` groups
//! (D3). PR-A defined the two primary kinds — [`Sandbox`] and
//! [`SandboxTemplate`] — to prove the derive pipeline compiles against the
//! pinned dependency tree; PR-C-1 expands their spec/status field sets to the
//! subset the broker actually reads and writes when managing a sandbox
//! lifecycle. The remaining kinds (`SandboxClaim`, `SandboxWarmPool`) land later.
//!
//! ## Why `podTemplate` is a `serde_json::Value`
//!
//! The broker reads the base `SandboxTemplate`'s `spec.podTemplate`,
//! deep-copies it, and shuffles a handful of keys (clears the `workspace`
//! volume, points it at a PVC/emptyDir, injects the per-session key volume). It
//! never *interprets* the pod spec, only passes it through byte-for-byte. Typing
//! the full [`k8s_openapi`] pod tree would add a large surface we don't reason
//! about and would require enabling k8s-openapi's optional `schemars` feature
//! (its `JsonSchema` impls are feature-gated, and the workspace does not enable
//! it). `serde_json::Value` round-trips any pod template exactly — treating it
//! as an opaque dict — while the fields the broker *does* reason
//! about (`operatingMode`, `shutdownPolicy`) are typed enums below.

#![forbid(unsafe_code)]

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Spec of a [`Sandbox`] — a running tenant sandbox managed by the broker.
///
/// `operatingMode` / `shutdownPolicy` are typed because the broker parks a
/// sandbox (`Suspended`) and resumes it (`Running`) and relies on
/// `shutdownPolicy: Retain` to keep the object across a park. `podTemplate` is
/// the opaque per-instance pod blueprint cloned from the backing
/// [`SandboxTemplate`] (see the module docs for why it is untyped).
#[derive(
    CustomResource,
    Debug,
    Clone,
    Default,
    PartialEq,
    Serialize,
    Deserialize,
    JsonSchema,
    utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
#[kube(
    group = "agents.x-k8s.io",
    version = "v1beta1",
    kind = "Sandbox",
    namespaced,
    status = "SandboxStatus"
)]
pub struct SandboxSpec {
    /// Name of the [`SandboxTemplate`] backing this sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_name: Option<String>,

    /// Lifecycle mode the upstream controller honours. `Running` (default) keeps
    /// the pod scheduled; `Suspended` parks it (pod deleted, object retained) so
    /// a later resume reuses the same identity + per-session Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operating_mode: Option<OperatingMode>,

    /// Whether the `Sandbox` object is retained after its pod exits. The broker
    /// sets `Retain` so a parked sandbox survives to be resumed rather than
    /// recreated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_policy: Option<ShutdownPolicy>,

    /// Opaque per-instance pod blueprint, cloned from the backing
    /// [`SandboxTemplate`] and shuffled by the broker (volumes, labels).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_template: Option<serde_json::Value>,
}

/// Spec of a [`SandboxTemplate`] — the reusable blueprint a [`Sandbox`] instantiates.
#[derive(CustomResource, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

    /// Pod blueprint the broker clones into each per-session [`Sandbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_template: Option<serde_json::Value>,
}

/// `spec.operatingMode` — the upstream controller's lifecycle mode.
///
/// Serializes as the upstream string literals `Running` / `Suspended` (serde's
/// default unit-variant representation), matching the values the Python broker
/// patches in `_set_sandbox_operating_mode`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema,
)]
pub enum OperatingMode {
    /// Pod scheduled and running (the broker's create-time default).
    Running,
    /// Parked: pod deleted, `Sandbox` object retained for resume.
    Suspended,
}

/// `spec.shutdownPolicy` — whether the `Sandbox` object survives its pod exiting.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema,
)]
pub enum ShutdownPolicy {
    /// Keep the object after the pod exits (the broker's create-time default).
    Retain,
    /// Delete the object when its pod exits.
    Delete,
}

/// A pod IP entry as surfaced in [`SandboxStatus::pod_i_ps`] (mirrors the
/// upstream `core.v1.PodIP` `{ ip }` shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema)]
pub struct PodIpEntry {
    /// The pod IP.
    pub ip: String,
}

/// A status condition as surfaced in [`SandboxStatus::conditions`] (mirrors the
/// upstream `meta.v1.Condition` subset the Python broker reads for readiness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema)]
pub struct SandboxCondition {
    /// Condition type (e.g. `Ready`).
    #[serde(rename = "type")]
    pub r#type: String,
    /// Condition status (e.g. `True` / `False` / `Unknown`).
    pub status: String,
    /// Machine-readable reason, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human-readable message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Last transition time (RFC 3339), if surfaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// Status of a [`Sandbox`] — the subset of fields the broker reads (readiness +
/// pod IP) plus the common scalars it forwards. Authored here so readiness can
/// be computed in typed Rust rather than by poking raw JSON.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    /// High-level lifecycle phase (`Running`, `Suspended`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,

    /// Pod IPs assigned to the sandbox; `[0]` is the address the broker proxies to.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "podIPs")]
    pub pod_i_ps: Option<Vec<String>>,

    /// Status conditions; a `Ready`/`True` entry means the sandbox is serving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<SandboxCondition>>,

    /// Convenience readiness flag, when the controller surfaces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,

    /// Human-readable status message, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl SandboxStatus {
    /// True when a `Ready`/`True` condition is present — the readiness
    /// predicate applied before proxying.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.conditions
            .iter()
            .flatten()
            .any(|c| c.r#type == "Ready" && c.status == "True")
    }

    /// First pod IP (the address the broker proxies to), or `None`.
    #[must_use]
    pub fn pod_ip(&self) -> Option<&str> {
        self.pod_i_ps
            .as_ref()
            .and_then(|ips| ips.first())
            .map(String::as_str)
    }
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
                operating_mode: None,
                shutdown_policy: None,
                pod_template: None,
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
                operating_mode: Some(OperatingMode::Running),
                shutdown_policy: Some(ShutdownPolicy::Retain),
                pod_template: None,
            },
        );
        let json = serde_json::to_string(&sandbox).expect("serialize");
        let back: Sandbox = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.spec.template_name.as_deref(), Some("base"));
        assert_eq!(back.spec.operating_mode, Some(OperatingMode::Running));
        assert_eq!(back.spec.shutdown_policy, Some(ShutdownPolicy::Retain));
    }

    #[test]
    fn sandbox_template_uses_extension_group() {
        let tmpl = SandboxTemplate::new(
            "base",
            SandboxTemplateSpec {
                description: Some("d".into()),
                pod_template: None,
            },
        );
        let value: serde_json::Value = serde_json::to_value(&tmpl).expect("serialize");
        assert_eq!(value["apiVersion"], "extensions.agents.x-k8s.io/v1beta1");
        assert_eq!(value["kind"], "SandboxTemplate");
    }

    #[test]
    fn operating_mode_serializes_as_upstream_string_literals() {
        assert_eq!(
            serde_json::to_string(&OperatingMode::Running).unwrap(),
            "\"Running\""
        );
        assert_eq!(
            serde_json::to_string(&OperatingMode::Suspended).unwrap(),
            "\"Suspended\""
        );
        assert_eq!(
            serde_json::from_str::<OperatingMode>("\"Running\"").unwrap(),
            OperatingMode::Running
        );
    }

    #[test]
    fn shutdown_policy_serializes_as_upstream_string_literals() {
        assert_eq!(
            serde_json::to_string(&ShutdownPolicy::Retain).unwrap(),
            "\"Retain\""
        );
        assert_eq!(
            serde_json::to_string(&ShutdownPolicy::Delete).unwrap(),
            "\"Delete\""
        );
    }

    #[test]
    fn sandbox_round_trips_a_full_fixture_with_pod_template_and_status() {
        // Mirrors the shape the broker POSTs + the controller status-populates.
        let raw = serde_json::json!({
            "apiVersion": "agents.x-k8s.io/v1beta1",
            "kind": "Sandbox",
            "metadata": {
                "name": "owui-c-abcdef012345",
                "namespace": "agent-sandbox-runtime",
                "labels": {
                    "app.kubernetes.io/managed-by": "owui-broker",
                    "broker-profile": "persistent"
                },
                "annotations": {
                    "broker-last-used": "1700000000",
                    "broker-user": "user-1",
                    "broker-session": "chat-1"
                }
            },
            "spec": {
                "operatingMode": "Running",
                "shutdownPolicy": "Retain",
                "podTemplate": {
                    "metadata": {"labels": {"profile": "persistent"}},
                    "spec": {"containers": [{"name": "sandbox", "image": "code-standard:latest"}]}
                }
            },
            "status": {
                "phase": "Running",
                "podIPs": ["10.0.0.5"],
                "conditions": [
                    {"type": "Ready", "status": "True"},
                    {"type": "PodScheduled", "status": "True"}
                ]
            }
        });

        let sbx: Sandbox = serde_json::from_value(raw).expect("deserialize fixture");
        assert_eq!(sbx.spec.operating_mode, Some(OperatingMode::Running));
        assert_eq!(sbx.spec.shutdown_policy, Some(ShutdownPolicy::Retain));
        assert_eq!(
            sbx.spec.pod_template.as_ref().unwrap()["spec"]["containers"][0]["image"],
            "code-standard:latest"
        );

        let status = sbx.status.clone().expect("status present");
        assert!(status.is_ready());
        assert_eq!(status.pod_ip(), Some("10.0.0.5"));

        // Round-trip back to JSON preserves the opaque pod template verbatim.
        let reserialized = serde_json::to_value(&sbx).expect("serialize");
        assert_eq!(
            reserialized["spec"]["podTemplate"]["spec"]["containers"][0]["image"],
            "code-standard:latest"
        );
    }

    #[test]
    fn status_readiness_requires_ready_true() {
        let not_ready = SandboxStatus {
            conditions: Some(vec![SandboxCondition {
                r#type: "Ready".into(),
                status: "False".into(),
                reason: None,
                message: None,
                last_transition_time: None,
            }]),
            ..Default::default()
        };
        assert!(!not_ready.is_ready());
        assert_eq!(not_ready.pod_ip(), None);

        let ready = SandboxStatus {
            conditions: Some(vec![SandboxCondition {
                r#type: "Ready".into(),
                status: "True".into(),
                reason: None,
                message: None,
                last_transition_time: None,
            }]),
            pod_i_ps: Some(vec!["10.0.0.6".into()]),
            ..Default::default()
        };
        assert!(ready.is_ready());
        assert_eq!(ready.pod_ip(), Some("10.0.0.6"));
    }

    #[test]
    fn empty_spec_omits_optional_fields() {
        let sbx = Sandbox::new("bare", SandboxSpec::default());
        let v = serde_json::to_value(&sbx).unwrap();
        // No optional spec keys should be emitted for a bare spec.
        assert!(v["spec"].as_object().unwrap().is_empty());
    }
}
