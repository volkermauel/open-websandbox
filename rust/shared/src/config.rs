//! Drop-in configuration parsing for the control plane (D12).
//!
//! The Python components today read their behaviour from environment variables
//! with fixed names; the Rust rewrite keeps those names and values identical so
//! the chart's env blocks are unchanged. PR-A surfaces two representative
//! fields to prove the `from_env` + serde-default pattern; the full config
//! object expands in PR-C.
//!
//! Note on testing: [`BrokerConfig::from_env`] reads the process environment
//! directly, and [`std::env::set_var`]/[`remove_var`](std::env::remove_var) are
//! `unsafe` since Rust 1.83 — incompatible with `#![forbid(unsafe_code)]`. The
//! pure parsing core is therefore factored into [`parse_value`], which the unit
//! tests exercise without touching the live environment.

#![forbid(unsafe_code)]

use std::env;
use std::fmt;
use std::str::FromStr;

/// Convenience alias for fallible operations whose callers don't need a typed
/// error. Used at the broker/runtime boundary in PR-B/PR-C.
pub type AnyResult<T> = anyhow::Result<T>;

/// Errors raised while loading configuration from the environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// An environment variable was present but could not be parsed into the
    /// declared type.
    #[error("invalid value for {var}: {message}")]
    Invalid { var: &'static str, message: String },
}

/// Broker configuration loaded from the environment.
///
/// Field names mirror the env-var names the Python broker already honours
/// (D12 — drop-in). PR-C fills in the remaining knobs (warm-pool sizing, leader
/// election, storage tiering, ...).
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
}

const fn default_max_terminal_sessions() -> u32 {
    8
}

const fn default_max_output_bytes() -> u64 {
    1_048_576 // 1 MiB
}

impl BrokerConfig {
    /// Load configuration from process environment variables, applying the same
    /// defaults as the Python implementation (D12).
    ///
    /// Returns [`ConfigError::Invalid`] when a recognised variable is set to a
    /// value that cannot be parsed into the declared type; absent variables fall
    /// back to their documented defaults.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            max_terminal_sessions: env_value("MAX_TERMINAL_SESSIONS")?
                .unwrap_or_else(default_max_terminal_sessions),
            max_output_bytes: env_value("MAX_OUTPUT_BYTES")?
                .unwrap_or_else(default_max_output_bytes),
        })
    }
}

/// Read an optional typed value from one environment variable.
///
/// Returns `Ok(None)` when the variable is absent or empty; returns
/// [`ConfigError::Invalid`] when it is present but malformed.
fn env_value<T>(var: &'static str) -> Result<Option<T>, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match env::var(var) {
        Ok(raw) if !raw.is_empty() => parse_value(var, &raw).map(Some),
        _ => Ok(None),
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
        message: format!("{raw:?} is not a valid {type}: {err}", type = std::any::type_name::<T>()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn serde_round_trips_explicit_values() {
        let json = r#"{"max_terminal_sessions":2,"max_output_bytes":512}"#;
        let cfg: BrokerConfig = serde_json::from_str(json).expect("explicit");
        assert_eq!(cfg.max_terminal_sessions, 2);
        assert_eq!(cfg.max_output_bytes, 512);
        let reserialized = serde_json::to_string(&cfg).expect("serialize");
        assert!(
            reserialized.contains("\"max_terminal_sessions\":2"),
            "{reserialized}"
        );
    }
}
