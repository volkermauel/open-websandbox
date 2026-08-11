//! Per-session runtime API-key minting + Secret shaping (PR-C-5 / issue #4).
//!
//! The runtime's hardened surface fail-closes unless a valid per-session API key
//! is mounted at `/etc/runtime-key/api-key` (volume `runtime-key`). The broker
//! owns that key's lifecycle: it mints one 256-bit key per sandbox, stores it in
//! a per-session Secret `owui-runtime-key-<sandbox>` (in the runtime namespace),
//! injects a read-only `secret` volume sourcing it into the sandbox pod, and
//! re-reads it on every proxied hop to authenticate to that runtime
//! (`Authorization: Bearer <key>`). One pod, one key — no shared runtime
//! credential crosses a sandbox boundary.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use kube::api::ObjectMeta;

/// Secret-name prefix.
pub const KEY_PREFIX: &str = "owui-runtime-key-";
/// The data key inside the Secret + the projected file name (`api-key`).
pub const DATA_KEY: &str = "api-key";

/// The per-session Secret name (`owui-runtime-key-<sandbox>`).
#[must_use]
pub fn secret_name(sandbox_name: &str) -> String {
    format!("{KEY_PREFIX}{sandbox_name}")
}

/// A fresh 256-bit per-session key, hex-encoded (64 chars). CSPRNG-sourced via
/// `rand::rng()` (ThreadRng / ChaCha12) — never a placeholder the runtime's
/// auth rejects. The encoding differs from a URL-safe token, but the entropy
/// and the contract — an opaque bearer string the runtime compares in constant
/// time — are identical.
#[must_use]
pub fn mint_key() -> String {
    use rand::Rng;
    let mut buf = [0u8; 32];
    // rand 0.10: `rand::rng()` -> ThreadRng (ChaCha12 CSPRNG, infallible). Was
    // `OsRng.try_fill_bytes(..)` on 0.9; the entropy contract is unchanged.
    rand::rng().fill_bytes(&mut buf);
    // Hex (no `base64` dep); 64 chars, 256 bits.
    let mut out = String::with_capacity(64);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Build the per-session key Secret (`stringData` so the apiserver base64-codes
/// it; we read it back via `data` on each hop). Labels:
/// `managed-by=owui-broker` + `component=runtime-key`.
#[must_use]
pub fn build_secret(sandbox_name: &str, namespace: &str, key: &str) -> Secret {
    let mut string_data = BTreeMap::new();
    string_data.insert(DATA_KEY.to_string(), key.to_string());
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "owui-broker".to_string(),
    );
    labels.insert("owui.io/component".to_string(), "runtime-key".to_string());
    Secret {
        metadata: ObjectMeta {
            name: Some(secret_name(sandbox_name)),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        string_data: Some(string_data),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    }
}

/// Inject the per-session runtime-key `secret` volume + a read-only mount at
/// `/etc/runtime-key` into a pod blueprint (`serde_json::Value`).
///
/// The volume is
/// a non-optional `secret` (not `projected`) sourcing `secretName` + an
/// `items` map (`api-key`→`api-key`); the mount is `readOnly`. The Secret must
/// exist before the pod is created (the caller `ensure_runtime_key`s first), so
/// the non-optional volume is satisfiable at pod-creation — the security
/// property that makes a missing key fail fast rather than mount empty.
pub fn inject_volume(pod: &mut serde_json::Value, sandbox_name: &str) {
    let secret_name = secret_name(sandbox_name);
    let Some(spec) = pod.get_mut("spec").and_then(|s| s.as_object_mut()) else {
        return;
    };
    // volumes += [{name: runtime-key, secret: {secretName, items: [{key,path}]}}]
    let volumes = spec
        .entry("volumes".to_string())
        .or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = volumes.as_array_mut() {
        let has = arr
            .iter()
            .any(|v| v.get("name").and_then(|n| n.as_str()) == Some("runtime-key"));
        if !has {
            arr.push(serde_json::json!({
                "name": "runtime-key",
                "secret": {
                    "secretName": secret_name,
                    "items": [{"key": DATA_KEY, "path": DATA_KEY}]
                }
            }));
        }
    }
    // containers[*].volumeMounts += [{name: runtime-key, mountPath: /etc/runtime-key, readOnly: true}]
    if let Some(containers) = spec.get_mut("containers").and_then(|c| c.as_array_mut()) {
        for c in containers {
            let Some(cobj) = c.as_object_mut() else {
                continue;
            };
            let mounts = cobj
                .entry("volumeMounts".to_string())
                .or_insert_with(|| serde_json::json!([]));
            if let Some(arr) = mounts.as_array_mut() {
                let has = arr
                    .iter()
                    .any(|m| m.get("name").and_then(|n| n.as_str()) == Some("runtime-key"));
                if !has {
                    arr.push(serde_json::json!({
                        "name": "runtime-key",
                        "mountPath": "/etc/runtime-key",
                        "readOnly": true
                    }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_name_has_runtime_key_prefix() {
        assert_eq!(
            secret_name("owui-abcdef012345"),
            "owui-runtime-key-owui-abcdef012345"
        );
    }

    #[test]
    fn mint_key_is_64_hex_chars_256_bits() {
        let k = mint_key();
        assert_eq!(k.len(), 64, "hex of 32 bytes = 64 chars");
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()), "hex only");
        // two mints differ (CSPRNG, not constant)
        assert_ne!(mint_key(), mint_key());
    }

    #[test]
    fn inject_volume_adds_secret_volume_and_readonly_mount() {
        let mut pod = serde_json::json!({
            "spec": {
                "containers": [{"name": "sandbox", "image": "x"}],
                "volumes": [{"name": "workspace", "emptyDir": {}}]
            }
        });
        inject_volume(&mut pod, "owui-deadbeef");
        let spec = &pod["spec"];
        // volume present, references the per-session secret + the items map
        let vol = spec["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "runtime-key")
            .expect("runtime-key volume injected");
        assert_eq!(
            vol["secret"]["secretName"],
            "owui-runtime-key-owui-deadbeef"
        );
        assert_eq!(vol["secret"]["items"][0]["key"], "api-key");
        assert_eq!(vol["secret"]["items"][0]["path"], "api-key");
        // mount present, readOnly at /etc/runtime-key
        let mount = spec["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "runtime-key")
            .expect("runtime-key mount injected");
        assert_eq!(mount["mountPath"], "/etc/runtime-key");
        assert_eq!(mount["readOnly"], true);
    }

    #[test]
    fn inject_volume_is_idempotent() {
        let mut pod = serde_json::json!({"spec": {"containers": [{"name": "sandbox"}]}});
        inject_volume(&mut pod, "owui-x");
        inject_volume(&mut pod, "owui-x");
        let n: usize = pod["spec"]["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["name"] == "runtime-key")
            .count();
        assert_eq!(n, 1, "not double-injected");
    }
}
