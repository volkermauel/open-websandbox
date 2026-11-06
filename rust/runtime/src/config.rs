//! Drop-in runtime configuration (D12 — same env-var names/values, so the
//! chart's env blocks are unchanged).
//!
//! [`RuntimeConfig::from_env`] reads the process environment directly; the pure
//! parsing core is factored into `parse_value` (mirroring `shared::config`) so
//! the unit tests exercise it without mutating the live environment. `set_var`
//! has been `unsafe` since Rust 1.83, which is incompatible with
//! `#![forbid(unsafe_code)]`.

#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

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
/// Default: 1 GiB per upload body (#162) — generous vs. axum's 2 MiB default,
/// still bounded well under the 2 GiB workspace quota.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
/// Max concurrent PTY terminal sessions (env `MAX_TERMINAL_SESSIONS`).
const DEFAULT_MAX_TERMINAL_SESSIONS: u32 = 8;
/// Shell used to run `/execute` commands (env `SHELL`).
const DEFAULT_SHELL: &str = "/bin/bash";
/// Projected-Secret volume holding the per-session API key (env
/// `RUNTIME_KEY_FILE`).
const DEFAULT_RUNTIME_KEY_FILE: &str = "/etc/runtime-key/api-key";
/// Scrollback captured per terminal, in bytes (env
/// `TERMINAL_SCROLLBACK_BYTES`). Replayed to a reattaching WS client and flushed
/// to the workspace on SIGTERM (issue #129); `0` disables capture, replay and flush.
const DEFAULT_TERMINAL_SCROLLBACK_BYTES: usize = 128 * 1024;
/// Seconds a detached terminal survives before the idle sweep reaps it.
const DEFAULT_TERMINAL_DETACH_TTL_SECS: u64 = 900;
/// Operator-provided environment info served by `GET /info` (env
/// `OPEN_TERMINAL_INFO` — upstream name kept for drop-in parity; empty
/// default = the route 404s like upstream's conditional registration).
const DEFAULT_INFO: &str = "";
/// Operator override for the `GET /system` prompt template (env
/// `OPEN_TERMINAL_SYSTEM_PROMPT` — upstream name; empty default = the
/// upstream-verbatim built-in prompt).
const DEFAULT_SYSTEM_PROMPT: &str = "";
// Capability-manifest knob (env `SANDBOX_TOOLS_MANIFEST`; workbench image
// sets it to `/usr/local/share/sandbox-capabilities.md`). Empty/unset =
// disabled — the prompt stays byte-for-byte upstream. The same knob also
// gates the config-driven `## Workspace conventions` section (built from
// `workdir`, not from the static file — WORKDIR varies per deployment).
/// Default desktop-sidecar base URL (env `DESKTOP_URL`). This is the value
/// the chart/sidecar lane sets; in code the lane activates only when the
/// variable is actually present (see [`RuntimeConfig::desktop_lane_active`]).
pub const DEFAULT_DESKTOP_URL: &str = "http://127.0.0.1:8000";
/// Seconds a sidecar `/api/config` fetch may take before it is abandoned
/// (the runtime's own flags are served unchanged when it does).
const DESKTOP_CONFIG_FETCH_TIMEOUT_SECS: u64 = 2;
/// Seconds a fetched sidecar `/api/config` stays cached.
const DESKTOP_CONFIG_CACHE_TTL_SECS: u64 = 5;

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
    /// Multipart upload body cap (bytes). axum's DefaultBodyLimit defaults to
    /// 2 MiB, which silently broke uploads > 2 MiB (#162) — the workspace
    /// quota is enforced separately at write time.
    pub max_upload_bytes: u64,
    /// Max concurrent PTY terminal sessions.
    pub max_terminal_sessions: u32,
    /// Shell binary for `/execute`.
    pub shell: String,
    /// Path to the projected-Secret per-session key file.
    pub runtime_key_file: PathBuf,
    /// Scrollback bytes captured per terminal (issue #129; 0 disables).
    pub terminal_scrollback_bytes: usize,
    /// Seconds a detached terminal survives before the idle sweep reaps it.
    pub terminal_detach_ttl_secs: u64,
    /// Operator info for `GET /info` (env `OPEN_TERMINAL_INFO`; empty ⇒ 404).
    pub info: String,
    /// `GET /system` prompt template override (env
    /// `OPEN_TERMINAL_SYSTEM_PROMPT`; empty ⇒ upstream-verbatim default).
    pub system_prompt: String,
    /// Capability-manifest path appended to the `GET /system` prompt (env
    /// `SANDBOX_TOOLS_MANIFEST`; `None` ⇒ disabled, prompt unchanged).
    pub tools_manifest: Option<PathBuf>,
    /// Desktop/browser sidecar base URL (env `DESKTOP_URL`, e.g. the default
    /// [`DEFAULT_DESKTOP_URL`]). `Some(_)` activates the sidecar lane: the
    /// HTTP fallback reverse-proxies unmatched requests to it and the explicit
    /// `/desktop/*`, `/events`, `/browser/view/*` WS routes relay to it.
    /// `None` (default) = lane off — the runtime behaves exactly as before
    /// (default 404 for unmatched paths, own flags only in `/api/config`).
    pub desktop_url: Option<String>,
    /// Shared bearer token for the sidecar hop (env `DESKTOP_API_KEY`). The
    /// runtime sends `Authorization: Bearer <key>` on every sidecar request
    /// (HTTP fallback + WS upgrades + config fetch); the inbound caller's
    /// Authorization is never forwarded. `None` = no Authorization injected.
    pub desktop_api_key: Option<String>,
    /// Operator kill switch for the desktop lane (env `DESKTOP_ENABLED`,
    /// `true`/`false`, default `true`). `false` disables the fallback, the WS
    /// relays and the config merge even when `DESKTOP_URL` is set.
    pub desktop_enabled: bool,
}

impl RuntimeConfig {
    /// True when the desktop-sidecar lane is active: a sidecar URL is
    /// configured AND the operator has not disabled it. Unset/empty env keeps
    /// the runtime byte-for-byte on its pre-desktop behaviour.
    #[must_use]
    pub fn desktop_lane_active(&self) -> bool {
        self.desktop_enabled && self.desktop_url.as_deref().is_some_and(|u| !u.is_empty())
    }

    /// Sidecar `/api/config` fetch timeout (short — a hung sidecar must never
    /// stall feature discovery).
    #[must_use]
    pub fn desktop_config_fetch_timeout(&self) -> Duration {
        Duration::from_secs(DESKTOP_CONFIG_FETCH_TIMEOUT_SECS)
    }

    /// How long a fetched sidecar config stays cached.
    #[must_use]
    pub fn desktop_config_cache_ttl(&self) -> Duration {
        Duration::from_secs(DESKTOP_CONFIG_CACHE_TTL_SECS)
    }
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
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            max_terminal_sessions: DEFAULT_MAX_TERMINAL_SESSIONS,
            shell: DEFAULT_SHELL.to_string(),
            runtime_key_file: PathBuf::from(DEFAULT_RUNTIME_KEY_FILE),
            terminal_scrollback_bytes: DEFAULT_TERMINAL_SCROLLBACK_BYTES,
            terminal_detach_ttl_secs: DEFAULT_TERMINAL_DETACH_TTL_SECS,
            info: DEFAULT_INFO.to_string(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            tools_manifest: None,
            desktop_url: None,
            desktop_api_key: None,
            desktop_enabled: true,
        }
    }
}

impl RuntimeConfig {
    /// Load configuration from process environment variables, applying the
    /// documented defaults (D12).
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_map(|name| env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Pure, testable core: build a config from a name→value lookup. Absent or
    /// empty values fall back to the documented defaults; present-but-malformed
    /// values fall back to the default as well (parse errors are swallowed), so a
    /// bad env var never crashes the runtime.
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
        cfg.max_upload_bytes = env_t(&get, "MAX_UPLOAD_BYTES", cfg.max_upload_bytes);
        cfg.max_terminal_sessions = env_t(&get, "MAX_TERMINAL_SESSIONS", cfg.max_terminal_sessions);
        if let Some(v) = get("SHELL") {
            cfg.shell = v;
        }
        if let Some(v) = get("RUNTIME_KEY_FILE") {
            cfg.runtime_key_file = PathBuf::from(v);
        }
        cfg.terminal_scrollback_bytes = env_t(
            &get,
            "TERMINAL_SCROLLBACK_BYTES",
            cfg.terminal_scrollback_bytes,
        );
        cfg.terminal_detach_ttl_secs = env_t(
            &get,
            "TERMINAL_DETACH_TTL_SECS",
            cfg.terminal_detach_ttl_secs,
        );
        if let Some(v) = get("OPEN_TERMINAL_INFO") {
            cfg.info = v;
        }
        if let Some(v) = get("OPEN_TERMINAL_SYSTEM_PROMPT") {
            cfg.system_prompt = v;
        }
        if let Some(v) = get("SANDBOX_TOOLS_MANIFEST") {
            cfg.tools_manifest = Some(PathBuf::from(v));
        }
        if let Some(v) = get("DESKTOP_URL") {
            cfg.desktop_url = Some(v);
        }
        if let Some(v) = get("DESKTOP_API_KEY") {
            cfg.desktop_api_key = Some(v);
        }
        cfg.desktop_enabled = env_t(&get, "DESKTOP_ENABLED", cfg.desktop_enabled);
        cfg
    }
}

/// Parse a raw string into `T`, returning `None` on failure.
///
/// Pure wrapper around [`FromStr`] so it can be unit-tested without mutating the
/// process environment (which would require `unsafe` since Rust 1.83). Parse
/// errors are swallowed rather than propagated.
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

/// Look up `var`, parse it, or fall back to `default` (parse errors are
/// swallowed).
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
    fn defaults_match_contract() {
        let cfg = RuntimeConfig::from_map(map(&[]));
        assert_eq!(cfg.workdir, PathBuf::from("/workspace"));
        assert_eq!(cfg.max_procs, 256);
        assert_eq!(cfg.max_output_bytes, 1_048_576);
        assert_eq!(cfg.default_timeout, 120);
        assert_eq!(cfg.max_timeout, 600);
        assert_eq!(cfg.max_workspace_bytes, 2_147_483_648);
        assert_eq!(cfg.max_upload_bytes, 1_073_741_824); // #162: 1 GiB, not axum's 2 MiB
        assert_eq!(cfg.max_terminal_sessions, 8);
        assert_eq!(cfg.shell, "/bin/bash");
        assert_eq!(
            cfg.runtime_key_file,
            PathBuf::from("/etc/runtime-key/api-key")
        );
        assert_eq!(cfg.info, "");
        assert_eq!(cfg.system_prompt, "");
        assert_eq!(cfg.tools_manifest, None);
        // Desktop lane: off unless DESKTOP_URL is actually set (unset env =
        // byte-for-byte pre-desktop behaviour), kill switch defaults open.
        assert_eq!(cfg.desktop_url, None);
        assert_eq!(cfg.desktop_api_key, None);
        assert!(cfg.desktop_enabled);
        assert!(!cfg.desktop_lane_active());
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
            ("MAX_UPLOAD_BYTES", "5000"),
            ("MAX_TERMINAL_SESSIONS", "4"),
            ("SHELL", "/bin/sh"),
            ("RUNTIME_KEY_FILE", "/keys/k"),
            ("OPEN_TERMINAL_INFO", "Physics dept sandbox"),
            ("OPEN_TERMINAL_SYSTEM_PROMPT", "host={{hostname}}"),
            (
                "SANDBOX_TOOLS_MANIFEST",
                "/usr/local/share/sandbox-capabilities.md",
            ),
            ("DESKTOP_URL", "http://127.0.0.1:8000"),
            ("DESKTOP_API_KEY", "sidecar-secret"),
        ]));
        assert_eq!(cfg.workdir, PathBuf::from("/tmp/ws"));
        assert_eq!(cfg.max_procs, 512);
        assert_eq!(cfg.max_output_bytes, 2048);
        assert_eq!(cfg.default_timeout, 30);
        assert_eq!(cfg.max_timeout, 90);
        assert_eq!(cfg.max_workspace_bytes, 1000);
        assert_eq!(cfg.max_upload_bytes, 5000);
        assert_eq!(cfg.max_terminal_sessions, 4);
        assert_eq!(cfg.shell, "/bin/sh");
        assert_eq!(cfg.runtime_key_file, PathBuf::from("/keys/k"));
        assert_eq!(cfg.info, "Physics dept sandbox");
        assert_eq!(cfg.system_prompt, "host={{hostname}}");
        assert_eq!(
            cfg.tools_manifest,
            Some(PathBuf::from("/usr/local/share/sandbox-capabilities.md"))
        );
        assert_eq!(
            cfg.desktop_url.as_deref(),
            Some("http://127.0.0.1:8000"),
            "DESKTOP_URL is stored verbatim"
        );
        assert_eq!(cfg.desktop_api_key.as_deref(), Some("sidecar-secret"));
        assert!(cfg.desktop_lane_active());
    }

    #[test]
    fn desktop_kill_switch_disables_lane() {
        let cfg = RuntimeConfig::from_map(map(&[
            ("DESKTOP_URL", "http://127.0.0.1:8000"),
            ("DESKTOP_ENABLED", "false"),
        ]));
        assert!(cfg.desktop_url.is_some());
        assert!(!cfg.desktop_enabled);
        assert!(!cfg.desktop_lane_active());
    }

    #[test]
    fn desktop_url_alone_activates_lane() {
        let cfg = RuntimeConfig::from_map(map(&[("DESKTOP_URL", DEFAULT_DESKTOP_URL)]));
        assert!(cfg.desktop_lane_active());
        assert_eq!(cfg.desktop_api_key, None);
        // Blank URL never activates (empty values are filtered like upstream).
        let blank = RuntimeConfig::from_map(map(&[("DESKTOP_URL", "")]));
        assert!(!blank.desktop_lane_active());
    }

    #[test]
    fn desktop_timeout_constants_are_short_and_cached() {
        let cfg = RuntimeConfig::from_map(map(&[]));
        assert_eq!(cfg.desktop_config_fetch_timeout(), Duration::from_secs(2));
        assert_eq!(cfg.desktop_config_cache_ttl(), Duration::from_secs(5));
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
