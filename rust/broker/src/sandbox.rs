//! Pure Sandbox lifecycle logic — no I/O, fully unit-testable.
//!
//! [`build_sandbox`] does the field surgery: it clones the backing template's
//! `podTemplate`, stamps the `profile` label onto it, and assembles a `Sandbox`
//! with the spec fields the broker always sets (`operatingMode: Running`,
//! `shutdownPolicy: Retain`) plus the managed-by label and the `broker-*`
//! annotations the reaper (C-2) and S3 tier (C-3) read. The per-session
//! runtime-key Secret volume injection and the persistent PVC `subPath` surgery
//! live in C-2/C-3 (per-session key management and the resolve-on-request flow
//! are out of scope for C-1); this helper produces the C-1 baseline that those
//! later passes extend.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use kube::ResourceExt;
use sha2::{Digest, Sha256};

use serde_json::Value;
use shared::{OperatingMode, Profile, Sandbox, SandboxSpec, SandboxTemplate, ShutdownPolicy};

use crate::error::ApiError;

/// Managed-by label key/value the broker stamps on every Sandbox it owns
/// (`app.kubernetes.io/managed-by=owui-broker`).
pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";
/// Managed-by label value the broker stamps on every Sandbox it owns.
pub const MANAGED_BY_VALUE: &str = "owui-broker";
/// Label key recording a Sandbox's persistence profile.
pub const PROFILE_LABEL_KEY: &str = "broker-profile";
/// Annotation key carrying the epoch-seconds "last used" timestamp the reaper
/// parks/reaps against.
pub const LAST_USED_KEY: &str = "broker-last-used";
/// #157: pending draft-adoption marker (value = draft sandbox name). Stamped
/// on a NEW chat sandbox when an adoption is planned; the move runs after
/// readiness and the marker is cleared — surviving claim retries that time
/// out before the sandbox is ready.
pub const DRAFT_ADOPT_PENDING_KEY: &str = "broker-draft-adopt-pending";
/// Annotation carrying the owning user id (S3 offload reads this).
pub const USER_KEY: &str = "broker-user";
/// Annotation carrying the owning session/chat id (S3 offload reads this).
pub const SESSION_KEY: &str = "broker-session";
/// Label key recording a persistent Sandbox's hot-tier backing
/// (`per-user-pvc` / `shared-subpath` / `s3-tiered`) — #140.
pub const PERSISTENT_MODE_LABEL_KEY: &str = "broker-persistent-mode";
/// Annotation on a claimed ephemeral Sandbox naming its owning
/// `SandboxClaim` — the reaper deletes the claim (cascade removes the
/// Sandbox; the pool replenishes) instead of the Sandbox directly.
pub const CLAIM_ANNOTATION_KEY: &str = "broker-claim";
/// Label marking a claimed ephemeral Sandbox as authenticating broker→pod
/// hops with the POOL key (`owui-pool-key`) — warm pods cannot carry a
/// per-session Secret (upstream adopts the running pod in place).
pub const AUTH_LABEL_KEY: &str = "owui.io/auth";
/// [`AUTH_LABEL_KEY`] value selecting pool-key hop auth.
pub const AUTH_POOL_KEY: &str = "pool-key";
/// Name of the pool-scoped runtime key Secret (chart-rendered, mounted by
/// the dedicated pool template).
pub const POOL_KEY_SECRET: &str = "owui-pool-key";
/// Annotation carrying the sha256 (hex) of the template fields cloned into
/// the Sandbox at creation (`owui.io/template-hash`) — the template-drift
/// self-heal compares it against the live [`SandboxTemplate`]. Absent on
/// legacy sandboxes (pre-stamping) and controller-built (claim-path)
/// sandboxes; legacy sandboxes with a copied `spec.podTemplate` fall back to
/// the podTemplate comparison in [`template_drifted`] instead.
pub const TEMPLATE_HASH_KEY: &str = "owui.io/template-hash";
/// Annotation naming the template the sandbox was created from
/// (`owui.io/template-name`). Drift is compared against THIS template: a
/// sandbox created from a non-base template must never be recycled into the
/// base one.
pub const TEMPLATE_NAME_KEY: &str = "owui.io/template-name";
/// Annotation the upstream agent-sandbox controller stamps on Sandboxes it
/// manages (`agents.x-k8s.io/sandbox-template-ref`), naming the template the
/// Sandbox was created from. The drift check consults it when the broker's
/// own [`TEMPLATE_NAME_KEY`] stamp is missing (sandboxes created before that
/// stamp existed — v0.3.0 and earlier).
pub const TEMPLATE_REF_ANNOTATION_KEY: &str = "agents.x-k8s.io/sandbox-template-ref";
/// Annotation stamped with the epoch seconds of the most recent
/// template-drift recycle of the (deterministically named) sandbox. Gates a
/// short anti-loop grace window: a freshly recycled sandbox is never
/// recycled again within [`crate::reaper::RECYCLE_GRACE_SECS`].
pub const RECYCLED_AT_KEY: &str = "broker-recycled-at";

/// Canonical JSON: object keys sorted recursively, so the serialization is
/// stable regardless of how the source was parsed/constructed (serde_json's
/// `Map` ordering is not a contract we own). The hash input for
/// [`template_hash`] must be identical across broker replicas and versions.
fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), canonical_json(v)))
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into(),
        serde_json::Value::Array(items) => {
            items.iter().map(canonical_json).collect::<Vec<_>>().into()
        }
        other => other.clone(),
    }
}

/// sha256 (hex) over the [`SandboxTemplate`] fields the broker copies into
/// each Sandbox — currently `spec.podTemplate`, the sole cloned field in the
/// CRD model (`spec.envVarsInjectionPolicy` /
/// `spec.volumeClaimTemplatesPolicy` do not exist on
/// [`shared::SandboxTemplateSpec`] yet; when the CRD gains them they must be
/// folded into this hash). `None` when the template carries no podTemplate —
/// such a template can never build a sandbox, so there is nothing to stamp.
#[must_use]
pub fn template_hash(template: &SandboxTemplate) -> Option<String> {
    let pod = template.spec.pod_template.as_ref()?;
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(pod).to_string().as_bytes());
    let digest = hasher.finalize();
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// The sandbox's `owui.io/template-hash` stamp (`None` when absent/empty —
/// legacy sandboxes fall back to the podTemplate comparison in
/// [`template_drifted`]).
#[must_use]
pub fn stamped_template_hash(sbx: &Sandbox) -> Option<String> {
    sbx.metadata
        .annotations
        .as_ref()?
        .get(TEMPLATE_HASH_KEY)
        .filter(|v| !v.is_empty())
        .cloned()
}

/// The template a sandbox was created from, resolved from its own metadata
/// in priority order: the broker's [`TEMPLATE_NAME_KEY`] stamp, then the
/// controller-stamped [`TEMPLATE_REF_ANNOTATION_KEY`] (present on sandboxes
/// created before the broker stamped its own name), else `None` — callers
/// fall back to the configured base template.
#[must_use]
pub fn template_name_of(sbx: &Sandbox) -> Option<&str> {
    let annots = sbx.metadata.annotations.as_ref()?;
    [TEMPLATE_NAME_KEY, TEMPLATE_REF_ANNOTATION_KEY]
        .iter()
        .find_map(|k| annots.get(*k).map(String::as_str).filter(|v| !v.is_empty()))
}

/// Whether `sbx` runs a stale copy of the LIVE template's pod blueprint —
/// the drift predicate behind the template-drift self-heal.
///
/// Two comparands, in priority order:
///
/// 1. the sandbox's [`TEMPLATE_HASH_KEY`] stamp vs the live template's hash
///    (the authoritative path for broker-created sandboxes);
/// 2. legacy sandboxes (pre-stamping, no stamp): their ACTUAL
///    `spec.podTemplate` vs the live template's `spec.podTemplate`, by
///    canonical-JSON equality (key-order free). The actual copy is
///    broker-mutated at creation (profile label, runtime-key volume, PVC
///    surgery), so a broker-created legacy sandbox normally reads as
///    drifted and is recycled once — the recreation carries the stamp and
///    the hash path takes over.
///
/// Every "cannot compare" case — a live template without a podTemplate, or
/// a sandbox without a copied podTemplate (controller-built/claim-bound,
/// whose spec the controller manages) — returns `false`. Callers must treat
/// `false` as "leave alone", never as "matches": recycling needs a positive
/// drift signal, never an inference from missing data.
#[must_use]
pub fn template_drifted(sbx: &Sandbox, live: &SandboxTemplate) -> bool {
    if let Some(old) = stamped_template_hash(sbx) {
        // A podTemplate-less live template cannot be hashed, so the stamp
        // cannot be compared — leave the sandbox alone.
        return template_hash(live).is_some_and(|new| new != old);
    }
    match (
        sbx.spec.pod_template.as_ref(),
        live.spec.pod_template.as_ref(),
    ) {
        (Some(actual), Some(pristine)) => canonical_json(actual) != canonical_json(pristine),
        _ => false,
    }
}

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

/// PVC hot-tier surgery (#140): repoint the `workspace` volume at the PVC
/// `claim_name` and give its mount the per-chat `sub_path`.
///
/// Mirrors the pre-rewrite broker: everything else in the cloned pod template
/// (image/env/resources/securityContext/runtimeClass) survives verbatim; only
/// the `workspace` volume source and its mount's `subPath` change. kubelet
/// creates a missing subPath directory inside the PVC, and the pod's
/// `fsGroup` makes it writable by the sandbox uid — no init container needed.
///
/// # Errors
///
/// [`ApiError::BadRequest`] when the template carries no `workspace` volume
/// or no container mounts it (the SandboxTemplate contract).
pub fn apply_persistent_volume(
    pod_template: &mut serde_json::Value,
    claim_name: &str,
    sub_path: &str,
) -> Result<(), ApiError> {
    let Some(spec) = pod_template.get_mut("spec").and_then(Value::as_object_mut) else {
        return Err(ApiError::BadRequest("pod template has no spec".to_string()));
    };
    let Some(volumes) = spec.get_mut("volumes").and_then(Value::as_array_mut) else {
        return Err(ApiError::BadRequest(
            "pod template has no spec.volumes — no workspace volume to repoint".to_string(),
        ));
    };
    let volume = volumes
        .iter_mut()
        .find(|v| v.get("name").and_then(Value::as_str) == Some("workspace"))
        .ok_or_else(|| {
            ApiError::BadRequest("no volume named 'workspace' in pod template".to_string())
        })?;
    *volume = serde_json::json!({
        "name": "workspace",
        "persistentVolumeClaim": {"claimName": claim_name}
    });

    let containers = spec
        .get_mut("containers")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::BadRequest("pod template has no spec.containers".to_string()))?;
    let mut patched = 0_usize;
    for container in containers {
        let Some(mounts) = container
            .get_mut("volumeMounts")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for mount in mounts {
            if mount.get("name").and_then(Value::as_str) == Some("workspace") {
                if let Some(obj) = mount.as_object_mut() {
                    obj.insert("subPath".to_string(), Value::String(sub_path.to_string()));
                    patched += 1;
                }
            }
        }
    }
    if patched == 0 {
        return Err(ApiError::BadRequest(
            "no container mounts the 'workspace' volume".to_string(),
        ));
    }
    Ok(())
}

/// Build a per-session `Sandbox` from a cloned template pod-blueprint.
///
/// `now` is taken as a parameter so this function is pure and deterministic in
/// tests; the handler
/// passes the current epoch seconds. `pod_template` is the template's pod
/// blueprint (the caller already [`extract_pod_template`]d it); this helper
/// stamps the `profile` label onto it.
#[allow(clippy::too_many_arguments)] // the broker-stamped field set is inherently wide
#[must_use]
pub fn build_sandbox(
    name: &str,
    user_id: Option<&str>,
    session_id: Option<&str>,
    profile: Profile,
    mut pod_template: serde_json::Value,
    namespace: &str,
    now: i64,
    template_name: Option<&str>,
    template_hash: Option<&str>,
) -> Sandbox {
    // Stamp the profile label onto the pod blueprint's metadata
    // (pod_tmpl.metadata.labels["profile"] = profile).
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
    if let Some(name) = template_name {
        annotations.insert(TEMPLATE_NAME_KEY.to_string(), name.to_string());
    }
    if let Some(hash) = template_hash {
        annotations.insert(TEMPLATE_HASH_KEY.to_string(), hash.to_string());
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
            Some("code-standard-v1"),
            Some("abc123"),
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
        assert_eq!(annots.get(TEMPLATE_HASH_KEY).unwrap(), "abc123");

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
        let sbx = build_sandbox(
            "noid",
            None,
            None,
            Profile::Ephemeral,
            pod,
            "ns",
            0,
            None,
            None,
        );
        let annots = sbx.metadata.annotations.as_ref().unwrap();
        assert!(!annots.contains_key(USER_KEY));
        assert!(!annots.contains_key(SESSION_KEY));
        assert!(!annots.contains_key(TEMPLATE_HASH_KEY));
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
        let sbx = build_sandbox(
            "x",
            None,
            None,
            Profile::Ephemeral,
            pod,
            "ns",
            1,
            None,
            None,
        );
        assert_eq!(
            sbx.spec.pod_template.as_ref().unwrap()["metadata"]["labels"]["profile"],
            "ephemeral"
        );
    }

    #[test]
    fn apply_persistent_volume_repoints_workspace_and_sets_subpath() {
        let mut pod = serde_json::json!({
            "spec": {
                "containers": [
                    {"name": "sandbox", "volumeMounts": [
                        {"name": "workspace", "mountPath": "/workspace"},
                        {"name": "home", "mountPath": "/home/sandbox"},
                    ]},
                    {"name": "sidecar", "volumeMounts": [
                        {"name": "workspace", "mountPath": "/workspace", "readOnly": true},
                    ]},
                ],
                "volumes": [
                    {"name": "workspace", "emptyDir": {}},
                    {"name": "home", "emptyDir": {}},
                ],
            }
        });
        apply_persistent_volume(&mut pod, "workspace-p-abc123", "chats/abc123").expect("surgery");

        let vols = &pod["spec"]["volumes"];
        assert_eq!(vols[0]["name"], "workspace");
        assert_eq!(
            vols[0]["persistentVolumeClaim"]["claimName"],
            "workspace-p-abc123"
        );
        assert!(
            vols[0].get("emptyDir").is_none(),
            "emptyDir must be replaced"
        );
        assert_eq!(vols[1]["name"], "home", "other volumes untouched");

        assert_eq!(
            pod["spec"]["containers"][0]["volumeMounts"][0]["subPath"],
            "chats/abc123"
        );
        assert!(pod["spec"]["containers"][0]["volumeMounts"][1]
            .get("subPath")
            .is_none());
        assert_eq!(
            pod["spec"]["containers"][1]["volumeMounts"][0]["subPath"],
            "chats/abc123"
        );
        assert_eq!(
            pod["spec"]["containers"][1]["volumeMounts"][0]["readOnly"],
            true
        );
    }

    #[test]
    fn apply_persistent_volume_fails_without_workspace_volume() {
        let mut pod = serde_json::json!({"spec": {
            "containers": [{"name": "sandbox", "volumeMounts": [{"name": "workspace", "mountPath": "/workspace"}]}],
            "volumes": [{"name": "other", "emptyDir": {}}],
        }});
        let err = apply_persistent_volume(&mut pod, "c", "s").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    #[test]
    fn apply_persistent_volume_fails_without_workspace_mount() {
        let mut pod = serde_json::json!({"spec": {
            "containers": [{"name": "sandbox", "volumeMounts": [{"name": "home", "mountPath": "/home"}]}],
            "volumes": [{"name": "workspace", "emptyDir": {}}],
        }});
        let err = apply_persistent_volume(&mut pod, "c", "s").unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
    }

    // --- template hash (drift self-heal stamp) -------------------------------

    #[test]
    fn template_hash_is_deterministic_and_key_order_independent() {
        let a = SandboxTemplate::new(
            "t",
            shared::SandboxTemplateSpec {
                description: None,
                pod_template: Some(serde_json::json!({
                    "spec": {"containers": [{"image": "img:1", "name": "sandbox"}]}
                })),
            },
        );
        // Same content, different construction order of the object keys.
        let b = SandboxTemplate::new(
            "t",
            shared::SandboxTemplateSpec {
                description: None,
                pod_template: Some(serde_json::json!({
                    "spec": {"containers": [{"name": "sandbox", "image": "img:1"}]}
                })),
            },
        );
        let ha = template_hash(&a).unwrap();
        assert_eq!(ha, template_hash(&a).unwrap(), "deterministic");
        assert_eq!(ha, template_hash(&b).unwrap(), "canonical (key-order free)");
        assert_eq!(ha.len(), 64, "sha256 hex");
    }

    #[test]
    fn template_hash_changes_with_pod_blueprint_and_ignores_description() {
        let mk = |image: &str, desc: Option<String>| {
            SandboxTemplate::new(
                "t",
                shared::SandboxTemplateSpec {
                    description: desc,
                    pod_template: Some(serde_json::json!({
                        "spec": {"containers": [{"name": "sandbox", "image": image}]}
                    })),
                },
            )
        };
        // Description is NOT cloned into the Sandbox → must not affect the hash.
        assert_eq!(
            template_hash(&mk("img:1", None)).unwrap(),
            template_hash(&mk("img:1", Some("edited docs".into()))).unwrap()
        );
        // An image bump IS a drift.
        assert_ne!(
            template_hash(&mk("img:1", None)).unwrap(),
            template_hash(&mk("img:2", None)).unwrap()
        );
    }

    #[test]
    fn template_hash_is_none_without_pod_template() {
        let t = SandboxTemplate::new(
            "empty",
            shared::SandboxTemplateSpec {
                description: None,
                pod_template: None,
            },
        );
        assert!(template_hash(&t).is_none());
    }
}
