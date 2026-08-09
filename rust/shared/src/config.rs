//! Drop-in configuration parsing for the control plane (D12).
//!
//! The Python components today read their behaviour from environment variables
//! with fixed names; the Rust rewrite keeps those names and values identical so
//! the chart's env blocks are unchanged. PR-A surfaced two representative
//! fields; PR-C-1 fills in the broker knobs needed for the HTTP surface and
//! Sandbox lifecycle: the shared Bearer secret, the namespace sandboxes live
//! in, the base template to clone, and the default persistence profile.
//!
//! Note on testing: [`BrokerConfig::from_env`] reads the process environment
//! directly, and [`std::env::set_var`]/[`remove_var`](std::env::remove_var) are
//! `unsafe` since Rust 1.83 — incompatible with `#![forbid(unsafe_code)]`. The
//! pure parsing core is therefore factored into [`BrokerConfig::from_map`],
//! which the unit tests exercise without touching the live environment.

#![forbid(unsafe_code)]

use std::env;
use std::fmt;
use std::str::FromStr;

/// Convenience alias for fallible operations whose callers don't need a typed
/// error. Used at the broker/runtime boundary.
pub type AnyResult<T> = anyhow::Result<T>;

/// Errors raised while loading configuration from the environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// An environment variable was present but could not be parsed into the
    /// declared type.
    #[error("invalid value for {var}: {message}")]
    Invalid { var: &'static str, message: String },
}

/// Known-unsafe placeholder values for `BROKER_SHARED_SECRET`. The broker's boot
/// guard and per-request auth treat these as "unset" (fail-closed), mirroring
/// the Python `_PLACEHOLDER_SECRETS` frozenset exactly.
pub const PLACEHOLDER_SECRETS: &[&str] = &[
    "",
    "dev-shared-secret-change-me",
    "change-me",
    "changeme",
    "placeholder",
];

/// True when `secret` is empty or a known placeholder (counts as "not
/// configured" — see [`PLACEHOLDER_SECRETS`]).
#[must_use]
pub fn is_placeholder_secret(secret: &str) -> bool {
    PLACEHOLDER_SECRETS.contains(&secret)
}

/// Persistence profile — what backing volume a sandbox gets.
///
/// Serializes as the lowercase literals `persistent` / `ephemeral` (D12 — same
/// values the Python broker honours in `BROKER_DEFAULT_PROFILE` and the
/// `X-Persistence` header). Defaults to [`Profile::Persistent`], matching the
/// Python deploy default.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Per-user/per-chat PVC-backed workspace; survives across sessions.
    #[default]
    Persistent,
    /// emptyDir workspace; destroyed when the sandbox is reaped.
    Ephemeral,
}

impl Profile {
    /// Lowercase wire value (`persistent` / `ephemeral`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Persistent => "persistent",
            Profile::Ephemeral => "ephemeral",
        }
    }
}

impl FromStr for Profile {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "persistent" => Ok(Profile::Persistent),
            "ephemeral" => Ok(Profile::Ephemeral),
            _ => Err(()),
        }
    }
}

/// Broker configuration loaded from the environment.
///
/// Field names mirror the env-var names the Python broker already honours
/// (D12 — drop-in). Later PRs add the remaining knobs (warm-pool sizing, leader
/// election, TTLs, storage tiering, ...).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BrokerConfig {
    /// Maximum concurrent terminal (PTY) sessions per runtime before new ones
    /// are rejected with HTTP 429 (env `MAX_TERMINAL_SESSIONS`, default `8`).
    #[serde(default = "default_max_terminal_sessions")]
    pub max_terminal_sessions: u32,

    /// Hard cap on captured process output in bytes (env `MAX_OUTPUT_BYTES`,
    /// default `1 MiB`).
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,

    /// Shared Bearer secret authenticating Open WebUI ↔ broker requests (env
    /// `BROKER_SHARED_SECRET`). Empty or a known placeholder counts as "not
    /// configured" → the broker fails closed (boot refuses, requests get 503).
    #[serde(default)]
    pub shared_secret: String,

    /// Namespace where broker-owned `Sandbox` objects live (env
    /// `BROKER_RUNTIME_NS`, default `agent-sandbox-runtime`).
    #[serde(default = "default_runtime_ns")]
    pub runtime_ns: String,

    /// Name of the base `SandboxTemplate` the broker clones per sandbox (env
    /// `BROKER_BASE_TEMPLATE`, default `code-standard-v1`).
    #[serde(default = "default_base_template")]
    pub base_template: String,

    /// Persistence profile used when a request omits an explicit override (env
    /// `BROKER_DEFAULT_PROFILE`, default `persistent`).
    #[serde(default)]
    pub default_profile: Profile,

    /// Shared broker→runtime API key the broker injects as `Authorization:
    /// Bearer <key>` on the direct pod hop (env `BROKER_RUNTIME_API_KEY`).
    ///
    /// PR-C-2 uses this single shared key; PR-C-3 replaces it with the per-session
    /// `owui-runtime-key-<sandbox>` Secret the Python broker mints/rotates (issue
    /// #4). Empty ⇒ no Authorization header is forwarded (the runtime then
    /// fail-closes), mirroring the Python `_runtime_auth_headers` `if key else {}`.
    #[serde(default)]
    pub runtime_api_key: String,

    /// Seconds to wait for a freshly created/resumed Sandbox to reach `Ready`
    /// (env `BROKER_CLAIM_TIMEOUT_SECONDS`, default `60`). Mirrors the Python
    /// `CLAIM_READY_TIMEOUT`. Expiry surfaces as HTTP 503 (sandbox unavailable).
    #[serde(default = "default_claim_timeout_seconds")]
    pub claim_timeout_seconds: u64,

    /// Total seconds allowed for one broker→runtime pod hop (env
    /// `BROKER_PROXY_TIMEOUT_SECONDS`, default `660`). Mirrors the Python
    /// `PROXY_TIMEOUT`; applied to the shared `reqwest` client.
    #[serde(default = "default_proxy_timeout_seconds")]
    pub proxy_timeout_seconds: u64,
}

const fn default_max_terminal_sessions() -> u32 {
    8
}

fn default_max_output_bytes() -> u64 {
    1_048_576 // 1 MiB
}

fn default_runtime_ns() -> String {
    "agent-sandbox-runtime".to_string()
}

fn default_base_template() -> String {
    "code-standard-v1".to_string()
}

const fn default_claim_timeout_seconds() -> u64 {
    60
}

const fn default_proxy_timeout_seconds() -> u64 {
    660
}

/// The all-defaults broker config (the same value `BrokerConfig::from_map(|_| None)`
/// yields), exposed as [`Default`] so tests can override single fields with
/// `BrokerConfig { shared_secret: "...", ..Default::default() }`.
impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            max_terminal_sessions: default_max_terminal_sessions(),
            max_output_bytes: default_max_output_bytes(),
            shared_secret: String::new(),
            runtime_ns: default_runtime_ns(),
            base_template: default_base_template(),
            default_profile: Profile::Persistent,
            runtime_api_key: String::new(),
            claim_timeout_seconds: default_claim_timeout_seconds(),
            proxy_timeout_seconds: default_proxy_timeout_seconds(),
        }
    }
}

impl BrokerConfig {
    /// Load configuration from process environment variables, applying the same
    /// defaults as the Python implementation (D12).
    ///
    /// Returns [`ConfigError::Invalid`] when a recognised numeric variable is
    /// set to a value that cannot be parsed; absent variables fall back to
    /// their documented defaults. A malformed `BROKER_DEFAULT_PROFILE` falls
    /// back to `persistent` (matching the Python defensive fallback) rather than
    /// erroring.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_map(|name| env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Pure, testable core: build a config from a name→value lookup. Absent or
    /// empty values fall back to the documented defaults; present-but-malformed
    /// numerics surface as [`ConfigError::Invalid`] (a malformed profile falls
    /// back to `persistent`).
    pub(crate) fn from_map<G>(get: G) -> Result<Self, ConfigError>
    where
        G: Fn(&str) -> Option<String>,
    {
        Ok(Self {
            max_terminal_sessions: env_value("MAX_TERMINAL_SESSIONS", &get)?
                .unwrap_or_else(default_max_terminal_sessions),
            max_output_bytes: env_value("MAX_OUTPUT_BYTES", &get)?
                .unwrap_or_else(default_max_output_bytes),
            shared_secret: get("BROKER_SHARED_SECRET").unwrap_or_default(),
            runtime_ns: get("BROKER_RUNTIME_NS").unwrap_or_else(default_runtime_ns),
            base_template: get("BROKER_BASE_TEMPLATE").unwrap_or_else(default_base_template),
            default_profile: get("BROKER_DEFAULT_PROFILE")
                .and_then(|raw| raw.parse().ok())
                .unwrap_or_default(),
            runtime_api_key: get("BROKER_RUNTIME_API_KEY").unwrap_or_default(),
            claim_timeout_seconds: env_value("BROKER_CLAIM_TIMEOUT_SECONDS", &get)?
                .unwrap_or_else(default_claim_timeout_seconds),
            proxy_timeout_seconds: env_value("BROKER_PROXY_TIMEOUT_SECONDS", &get)?
                .unwrap_or_else(default_proxy_timeout_seconds),
        })
    }
}

/// Read an optional typed value from one environment variable.
///
/// Returns `Ok(None)` when the variable is absent or empty; returns
/// [`ConfigError::Invalid`] when it is present but malformed.
fn env_value<T, G>(var: &'static str, get: &G) -> Result<Option<T>, ConfigError>
where
    G: Fn(&str) -> Option<String>,
    T: FromStr,
    T::Err: fmt::Display,
{
    match get(var) {
        Some(raw) => parse_value(var, &raw).map(Some),
        None => Ok(None),
    }
}

/// Parse a raw string into `T`, attributing failures to `var`.
///
/// Pure wrapper around [`FromStr`] so it can be unit-tested without mutating the
/// process environment (which would require `unsafe` since Rust 1.83).
fn parse_value<T>(var: &'static str, raw: &str) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    raw.parse::<T>().map_err(|err| ConfigError::Invalid {
        var,
        message: format!(
            "{raw:?} is not a valid {type}: {err}",
            type = std::any::type_name::<T>()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn parse_value_accepts_valid_input() {
        let n: u32 = parse_value("MAX_TERMINAL_SESSIONS", "4").expect("valid");
        assert_eq!(n, 4);
        let b: u64 = parse_value("MAX_OUTPUT_BYTES", "2097152").expect("valid");
        assert_eq!(b, 2_097_152);
    }

    #[test]
    fn parse_value_rejects_garbage_with_var_name() {
        let err = parse_value::<u32>("MAX_TERMINAL_SESSIONS", "not-a-number").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    var: "MAX_TERMINAL_SESSIONS",
                    ..
                }
            ),
            "wrong variant: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("MAX_TERMINAL_SESSIONS"), "{msg}");
        assert!(msg.contains("not-a-number"), "{msg}");
        assert!(msg.contains("u32"), "{msg}");
    }

    #[test]
    fn serde_defaults_match_documented_values() {
        // A missing config section must deserialize to the documented defaults,
        // so chart env blocks that omit these keys behave identically to today.
        let cfg: BrokerConfig = serde_json::from_str("{}").expect("empty object");
        assert_eq!(cfg.max_terminal_sessions, 8);
        assert_eq!(cfg.max_output_bytes, 1_048_576);
        assert_eq!(cfg.shared_secret, "");
        assert_eq!(cfg.runtime_ns, "agent-sandbox-runtime");
        assert_eq!(cfg.base_template, "code-standard-v1");
        assert_eq!(cfg.default_profile, Profile::Persistent);
    }

    #[test]
    fn serde_defaults_cover_new_proxy_fields() {
        let cfg: BrokerConfig = serde_json::from_str("{}").expect("empty object");
        assert_eq!(cfg.runtime_api_key, "");
        assert_eq!(cfg.claim_timeout_seconds, 60);
        assert_eq!(cfg.proxy_timeout_seconds, 660);
    }

    #[test]
    fn serde_round_trips_explicit_values() {
        let cfg = BrokerConfig {
            max_terminal_sessions: 2,
            max_output_bytes: 512,
            shared_secret: "s3cret".into(),
            runtime_ns: "ns".into(),
            base_template: "tmpl".into(),
            default_profile: Profile::Ephemeral,
            runtime_api_key: "rt-key".into(),
            claim_timeout_seconds: 30,
            proxy_timeout_seconds: 99,
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        assert!(json.contains("\"max_terminal_sessions\":2"), "{json}");
        assert!(json.contains("\"default_profile\":\"ephemeral\""), "{json}");
        assert!(json.contains("\"runtime_api_key\":\"rt-key\""), "{json}");
        assert!(json.contains("\"claim_timeout_seconds\":30"), "{json}");
        let back: BrokerConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cfg);
    }

    #[test]
    fn from_map_reads_broker_env_vars() {
        let cfg = BrokerConfig::from_map(map(&[
            ("BROKER_SHARED_SECRET", "hunter2"),
            ("BROKER_RUNTIME_NS", "sandbox-ns"),
            ("BROKER_BASE_TEMPLATE", "code-standard-v2"),
            ("BROKER_DEFAULT_PROFILE", "ephemeral"),
            ("BROKER_RUNTIME_API_KEY", "rt-key"),
            ("BROKER_CLAIM_TIMEOUT_SECONDS", "42"),
            ("BROKER_PROXY_TIMEOUT_SECONDS", "300"),
            ("MAX_TERMINAL_SESSIONS", "4"),
        ]))
        .expect("ok");
        assert_eq!(cfg.shared_secret, "hunter2");
        assert_eq!(cfg.runtime_ns, "sandbox-ns");
        assert_eq!(cfg.base_template, "code-standard-v2");
        assert_eq!(cfg.default_profile, Profile::Ephemeral);
        assert_eq!(cfg.runtime_api_key, "rt-key");
        assert_eq!(cfg.claim_timeout_seconds, 42);
        assert_eq!(cfg.proxy_timeout_seconds, 300);
        assert_eq!(cfg.max_terminal_sessions, 4);
    }

    #[test]
    fn from_map_defaults_when_env_absent() {
        let cfg = BrokerConfig::from_map(map(&[])).expect("ok");
        assert_eq!(cfg.shared_secret, "");
        assert_eq!(cfg.runtime_ns, "agent-sandbox-runtime");
        assert_eq!(cfg.base_template, "code-standard-v1");
        assert_eq!(cfg.default_profile, Profile::Persistent);
        assert_eq!(cfg.runtime_api_key, "");
        assert_eq!(cfg.claim_timeout_seconds, 60);
        assert_eq!(cfg.proxy_timeout_seconds, 660);
    }

    #[test]
    fn from_map_bad_profile_falls_back_to_persistent() {
        let cfg = BrokerConfig::from_map(map(&[("BROKER_DEFAULT_PROFILE", "nope")]))
            .expect("falls back, not errors");
        assert_eq!(cfg.default_profile, Profile::Persistent);
    }

    #[test]
    fn profile_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Profile::Persistent).unwrap(),
            "\"persistent\""
        );
        assert_eq!(
            serde_json::to_string(&Profile::Ephemeral).unwrap(),
            "\"ephemeral\""
        );
        assert_eq!(
            serde_json::from_str::<Profile>("\"ephemeral\"").unwrap(),
            Profile::Ephemeral
        );
    }

    #[test]
    fn placeholder_detection() {
        for p in PLACEHOLDER_SECRETS {
            assert!(is_placeholder_secret(p), "{p:?} should be placeholder");
        }
        assert!(!is_placeholder_secret("a-strong-shared-secret-123456"));
    }
}
