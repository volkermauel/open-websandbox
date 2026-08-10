//! Pure Sandbox lifecycle logic — no I/O, fully unit-testable.
//!
//! [`build_sandbox`] mirrors the Python `_create_sandbox`'s field surgery: it
//! clones the backing template's `podTemplate`, stamps the `profile` label onto
//! it, and assembles a `Sandbox` with the spec fields the broker always sets
//! (`operatingMode: Running`, `shutdownPolicy: Retain`) plus the managed-by
//! label and the `broker-*` annotations the reaper (C-2) and S3 tier (C-3)
//! read. The per-session runtime-key Secret volume injection and the persistent
//! PVC `subPath` surgery live in C-2/C-3 (per-session key management and the
//! resolve-on-request flow are out of scope for C-1); this helper produces the
//! C-1 baseline that those later passes extend.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use kube::ResourceExt;

use shared::{OperatingMode, Profile, Sandbox, SandboxSpec, SandboxTemplate, ShutdownPolicy};

use crate::error::ApiError;

/// Managed-by label key/value the broker stamps on every Sandbox it owns
/// (mirrors the Python `MANAGED_BY = {"app.kubernetes.io/managed-by": "owui-broker"}`).
pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY_VALUE: &str = "owui-broker";
/// Label key recording a Sandbox's persistence profile (Python `PROFILE` const).
pub const PROFILE_LABEL_KEY: &str = "broker-profile";
/// Annotation key carrying the epoch-seconds "last used" timestamp the reaper
/// parks/reaps against (Python `LAST_USED` const).
pub const LAST_USED_KEY: &str = "broker-last-used";
/// Annotation carrying the owning user id (S3 offload reads this).
pub const USER_KEY: &str = "broker-user";
/// Annotation carrying the owning session/chat id (S3 offload reads this).
pub const SESSION_KEY: &str = "broker-session";

/// Extract the template's `podTemplate` (opaque JSON) to clone into a Sandbox.
///
/// Returns [`ApiError::BadRequest`] when the template has no `podTemplate` —
/// every usable template carries one, so its absence is a caller/template error.
pub fn extract_pod_template(template: &SandboxTemplate) -> Result<serde_json::Value, ApiError> {
    template.spec.pod_template.clone().ok_or_else(|| {
        ApiError::BadRequest(format!(
            "template {} has no spec.podTemplate",
            template.name_any()
        ))
    })
}

/// Build a per-session `Sandbox` from a cloned template pod-blueprint.
///
/// Mirrors the field set the Python `_create_sandbox` writes. `now` is taken as
/// a parameter so this function is pure and deterministic in tests; the handler
/// passes the current epoch seconds. `pod_template` is the template's pod
/// blueprint (the caller already [`extract_pod_template`]d it); this helper
/// stamps the `profile` label onto it.
#[must_use]
pub fn build_sandbox(
    name: &str,
    user_id: Option<&str>,
    session_id: Option<&str>,
    profile: Profile,
    mut pod_template: serde_json::Value,
    namespace: &str,
    now: i64,
) -> Sandbox {
    // Stamp the profile label onto the pod blueprint's metadata (Python:
    // pod_tmpl.metadata.labels["profile"] = profile).
    if let Some(obj) = pod_template.as_object_mut() {
        let metadata = obj
            .entry("metadata")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(meta) = metadata.as_object_mut() {
            let labels = meta
                .entry("labels")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(lbls) = labels.as_object_mut() {
                lbls.insert("profile".to_string(), serde_json::json!(profile.as_str()));
            }
        }
    }

    // PR-C-5 / #4: inject the per-session runtime-key Secret volume + a
    // read-only mount at /etc/runtime-key. The Secret (owui-runtime-key-<name>)
    // is ensured by the caller before the Sandbox is created, so the
    // non-optional volume is satisfiable when the controller schedules the pod.
    crate::runtime_key::inject_volume(&mut pod_template, name);

    let mut labels = BTreeMap::new();
    labels.insert(MANAGED_BY_KEY.to_string(), MANAGED_BY_VALUE.to_string());
    labels.insert(PROFILE_LABEL_KEY.to_string(), profile.as_str().to_string());

    let mut annotations = BTreeMap::new();
    annotations.insert(LAST_USED_KEY.to_string(), now.to_string());
    if let Some(user) = user_id {
        annotations.insert(USER_KEY.to_string(), user.to_string());
    }
    if let Some(session) = session_id {
        annotations.insert(SESSION_KEY.to_string(), session.to_string());
    }

    let mut sandbox = Sandbox::new(
        name,
        SandboxSpec {
            template_name: None,
            operating_mode: Some(OperatingMode::Running),
            shutdown_policy: Some(ShutdownPolicy::Retain),
            pod_template: Some(pod_template),
        },
    );
    sandbox.metadata.namespace = Some(namespace.to_string());
    sandbox.metadata.labels = Some(labels);
    sandbox.metadata.annotations = Some(annotations);
    sandbox
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_with_pod() -> SandboxTemplate {
        SandboxTemplate::new(
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

    #[test]
    fn build_sandbox_sets_the_broker_field_set() {
        let tmpl = template_with_pod();
        let pod = extract_pod_template(&tmpl).unwrap();
        let sbx = build_sandbox(
            "owui-c-abcdef",
            Some("user-1"),
            Some("chat-1"),
            Profile::Persistent,
            pod,
            "agent-sandbox-runtime",
            1_700_000_000,
        );

        assert_eq!(sbx.name_any(), "owui-c-abcdef");
        assert_eq!(
            sbx.metadata.namespace.as_deref(),
            Some("agent-sandbox-runtime")
        );
        let labels = sbx.metadata.labels.as_ref().unwrap();
        assert_eq!(labels.get(MANAGED_BY_KEY).unwrap(), MANAGED_BY_VALUE);
        assert_eq!(labels.get(PROFILE_LABEL_KEY).unwrap(), "persistent");
        let annots = sbx.metadata.annotations.as_ref().unwrap();
        assert_eq!(annots.get(LAST_USED_KEY).unwrap(), "1700000000");
        assert_eq!(annots.get(USER_KEY).unwrap(), "user-1");
        assert_eq!(annots.get(SESSION_KEY).unwrap(), "chat-1");

        assert_eq!(sbx.spec.operating_mode, Some(OperatingMode::Running));
        assert_eq!(sbx.spec.shutdown_policy, Some(ShutdownPolicy::Retain));

        // The profile label was stamped onto the cloned pod template, and the
        // original container/volume blueprint survives verbatim.
        let pod = sbx.spec.pod_template.as_ref().unwrap();
        assert_eq!(pod["metadata"]["labels"]["profile"], "persistent");
        assert_eq!(
            pod["spec"]["containers"][0]["image"],
            "code-standard:latest"
        );
        assert_eq!(pod["spec"]["volumes"][0]["name"], "workspace");
    }

    #[test]
    fn build_sandbox_omits_user_session_when_absent() {
        let tmpl = template_with_pod();
        let pod = extract_pod_template(&tmpl).unwrap();
        let sbx = build_sandbox("noid", None, None, Profile::Ephemeral, pod, "ns", 0);
        let annots = sbx.metadata.annotations.as_ref().unwrap();
        assert!(!annots.contains_key(USER_KEY));
        assert!(!annots.contains_key(SESSION_KEY));
        assert_eq!(
            sbx.metadata
                .labels
                .as_ref()
                .unwrap()
                .get(PROFILE_LABEL_KEY)
                .unwrap(),
            "ephemeral"
        );
    }

    #[test]
    fn extract_pod_template_errors_when_absent() {
        let tmpl = SandboxTemplate::new(
            "empty",
            shared::SandboxTemplateSpec {
                description: None,
                pod_template: None,
            },
        );
        let err = extract_pod_template(&tmpl).unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn build_sandbox_stamps_profile_label_onto_metadataless_template() {
        // A podTemplate with no metadata must still get the profile label.
        let pod = serde_json::json!({"spec": {"containers": []}});
        let sbx = build_sandbox("x", None, None, Profile::Ephemeral, pod, "ns", 1);
        assert_eq!(
            sbx.spec.pod_template.as_ref().unwrap()["metadata"]["labels"]["profile"],
            "ephemeral"
        );
    }
}
