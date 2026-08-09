//! Drop-in runtime configuration (D12 — same env-var names/values as the Python
//! runtime, so the chart's env blocks are unchanged).
//!
//! [`RuntimeConfig::from_env`] reads the process environment directly; the pure
//! parsing core is factored into [`parse_value`] (mirroring `shared::config`) so
//! the unit tests exercise it without mutating the live environment. `set_var`
//! has been `unsafe` since Rust 1.83, which is incompatible with
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::str::FromStr;

/// Default workspace root (env `WORKDIR`).
const DEFAULT_WORKDIR: &str = "/workspace";
/// Per-sandbox process cap applied as `RLIMIT_NPROC` (env `MAX_PROCS`).
const DEFAULT_MAX_PROCS: u64 = 256;
/// Per-stream captured-output cap, in bytes (env `MAX_OUTPUT_BYTES`, 1 MiB).
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Default command timeout, in seconds (env `DEFAULT_TIMEOUT`).
const DEFAULT_DEFAULT_TIMEOUT: u64 = 120;
/// Hard ceiling on command timeout, in seconds (env `MAX_TIMEOUT`).
const DEFAULT_MAX_TIMEOUT: u64 = 600;
/// Workspace size cap for snapshot/restore pre-check, in bytes (env
/// `MAX_WORKSPACE_BYTES`, 2 GiB).
const DEFAULT_MAX_WORKSPACE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Max concurrent PTY terminal sessions (env `MAX_TERMINAL_SESSIONS`).
const DEFAULT_MAX_TERMINAL_SESSIONS: u32 = 8;
/// Shell used to run `/execute` commands (env `SHELL`).
const DEFAULT_SHELL: &str = "/bin/bash";
/// Projected-Secret volume holding the per-session API key (env
/// `RUNTIME_KEY_FILE`).
const DEFAULT_RUNTIME_KEY_FILE: &str = "/etc/runtime-key/api-key";

/// Runtime configuration loaded from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Workspace root every file op is confined to.
    pub workdir: PathBuf,
    /// `RLIMIT_NPROC` applied to the runtime process (inherited by children).
    pub max_procs: u64,
    /// Per-stream output cap (bytes / code points — see `execute::cap`).
    pub max_output_bytes: usize,
    /// Default `/execute` timeout (seconds).
    pub default_timeout: u64,
    /// Maximum `/execute` timeout (seconds).
    pub max_timeout: u64,
    /// Snapshot/restore size cap (bytes).
    pub max_workspace_bytes: u64,
    /// Max concurrent PTY terminal sessions.
    pub max_terminal_sessions: u32,
    /// Shell binary for `/execute`.
    pub shell: String,
    /// Path to the projected-Secret per-session key file.
    pub runtime_key_file: PathBuf,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            workdir: PathBuf::from(DEFAULT_WORKDIR),
            max_procs: DEFAULT_MAX_PROCS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            default_timeout: DEFAULT_DEFAULT_TIMEOUT,
            max_timeout: DEFAULT_MAX_TIMEOUT,
            max_workspace_bytes: DEFAULT_MAX_WORKSPACE_BYTES,
            max_terminal_sessions: DEFAULT_MAX_TERMINAL_SESSIONS,
            shell: DEFAULT_SHELL.to_string(),
            runtime_key_file: PathBuf::from(DEFAULT_RUNTIME_KEY_FILE),
        }
    }
}

impl RuntimeConfig {
    /// Load configuration from process environment variables, applying the same
    /// defaults as the Python implementation (D12).
    pub fn from_env() -> Self {
        Self::from_map(|name| env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Pure, testable core: build a config from a name→value lookup. Absent or
    /// empty values fall back to the documented defaults; present-but-malformed
    /// values fall back to the default as well (the Python `_env_int` swallows
    /// `TypeError`/`ValueError`), so a bad env var never crashes the runtime.
    pub(crate) fn from_map<G>(get: G) -> Self
    where
        G: Fn(&str) -> Option<String>,
    {
        let mut cfg = Self::default();
        if let Some(v) = get("WORKDIR") {
            cfg.workdir = PathBuf::from(v);
        }
        cfg.max_procs = env_t(&get, "MAX_PROCS", cfg.max_procs);
        cfg.max_output_bytes = env_t(&get, "MAX_OUTPUT_BYTES", cfg.max_output_bytes);
        cfg.default_timeout = env_t(&get, "DEFAULT_TIMEOUT", cfg.default_timeout);
        cfg.max_timeout = env_t(&get, "MAX_TIMEOUT", cfg.max_timeout);
        cfg.max_workspace_bytes = env_t(&get, "MAX_WORKSPACE_BYTES", cfg.max_workspace_bytes);
        cfg.max_terminal_sessions = env_t(&get, "MAX_TERMINAL_SESSIONS", cfg.max_terminal_sessions);
        if let Some(v) = get("SHELL") {
            cfg.shell = v;
        }
        if let Some(v) = get("RUNTIME_KEY_FILE") {
            cfg.runtime_key_file = PathBuf::from(v);
        }
        cfg
    }
}

/// Parse a raw string into `T`, returning `None` on failure.
///
/// Pure wrapper around [`FromStr`] so it can be unit-tested without mutating the
/// process environment (which would require `unsafe` since Rust 1.83). Mirrors
/// the Python `_env_int` swallow-on-error behaviour.
pub(crate) fn parse_value<T>(var: &'static str, raw: &str) -> Option<T>
where
    T: FromStr,
{
    raw.parse::<T>().ok().or_else(|| {
        tracing::warn!(
            var,
            raw,
            type_name = std::any::type_name::<T>(),
            "ignoring malformed runtime env var; using default"
        );
        None
    })
}

/// Look up `var`, parse it, or fall back to `default` (mirrors the Python
/// `_env_int` swallow-on-error behaviour).
fn env_t<G, T>(get: &G, var: &'static str, default: T) -> T
where
    G: Fn(&str) -> Option<String>,
    T: FromStr,
{
    get(var)
        .and_then(|raw| parse_value(var, &raw))
        .unwrap_or(default)
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
    fn defaults_match_python() {
        let cfg = RuntimeConfig::from_map(map(&[]));
        assert_eq!(cfg.workdir, PathBuf::from("/workspace"));
        assert_eq!(cfg.max_procs, 256);
        assert_eq!(cfg.max_output_bytes, 1_048_576);
        assert_eq!(cfg.default_timeout, 120);
        assert_eq!(cfg.max_timeout, 600);
        assert_eq!(cfg.max_workspace_bytes, 2_147_483_648);
        assert_eq!(cfg.max_terminal_sessions, 8);
        assert_eq!(cfg.shell, "/bin/bash");
        assert_eq!(
            cfg.runtime_key_file,
            PathBuf::from("/etc/runtime-key/api-key")
        );
    }

    #[test]
    fn parses_explicit_values() {
        let cfg = RuntimeConfig::from_map(map(&[
            ("WORKDIR", "/tmp/ws"),
            ("MAX_PROCS", "512"),
            ("MAX_OUTPUT_BYTES", "2048"),
            ("DEFAULT_TIMEOUT", "30"),
            ("MAX_TIMEOUT", "90"),
            ("MAX_WORKSPACE_BYTES", "1000"),
            ("MAX_TERMINAL_SESSIONS", "4"),
            ("SHELL", "/bin/sh"),
            ("RUNTIME_KEY_FILE", "/keys/k"),
        ]));
        assert_eq!(cfg.workdir, PathBuf::from("/tmp/ws"));
        assert_eq!(cfg.max_procs, 512);
        assert_eq!(cfg.max_output_bytes, 2048);
        assert_eq!(cfg.default_timeout, 30);
        assert_eq!(cfg.max_timeout, 90);
        assert_eq!(cfg.max_workspace_bytes, 1000);
        assert_eq!(cfg.max_terminal_sessions, 4);
        assert_eq!(cfg.shell, "/bin/sh");
        assert_eq!(cfg.runtime_key_file, PathBuf::from("/keys/k"));
    }

    #[test]
    fn malformed_values_fall_back_to_defaults() {
        let cfg = RuntimeConfig::from_map(map(&[
            ("MAX_PROCS", "not-a-number"),
            ("MAX_TIMEOUT", "-5"),
            ("DEFAULT_TIMEOUT", ""),
        ]));
        assert_eq!(cfg.max_procs, 256);
        assert_eq!(cfg.max_timeout, 600);
        assert_eq!(cfg.default_timeout, 120);
    }

    #[test]
    fn parse_value_helper() {
        assert_eq!(parse_value::<u64>("X", "42"), Some(42));
        assert_eq!(parse_value::<u64>("X", "bad"), None);
    }
}
