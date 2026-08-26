//! Drop-in configuration parsing for the control plane (D12).
//!
//! The control plane reads its behaviour from environment variables with
//! fixed names; the Rust rewrite keeps those names and values identical so the
//! chart's env blocks are unchanged. PR-A surfaced two representative fields;
//! PR-C-1 filled in the broker knobs needed for the HTTP surface and Sandbox
//! lifecycle; PR-C-3 (this pass) adds the idle-reaper TTLs + the leader-election
//! lease parameters the reaper loop + lease loop read (`IDLE_TTL` / `PARK_TTL` /
//! `REAP_TTL` / `REAPER_POLL_SECONDS` + `_LEADER_*`).
//!
//! Note on testing: [`BrokerConfig::from_env`] reads the process environment
//! directly, and [`std::env::set_var`]/[`remove_var`](std::env::remove_var) are
//! `unsafe` since Rust 1.83 — incompatible with `#![forbid(unsafe_code)]`. The
//! pure parsing core is therefore factored into `BrokerConfig::from_map`,
//! which the unit tests exercise without touching the live environment.

#![forbid(unsafe_code)]

use std::env;
use std::fmt;
use std::str::FromStr;

/// Errors raised while loading configuration from the environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// An environment variable was present but could not be parsed into the
    /// declared type.
    #[error("invalid value for {var}: {message}")]
    Invalid {
        /// The environment variable name whose value failed to parse.
        var: &'static str,
        /// Human-readable reason the value was rejected.
        message: String,
    },
}

/// Known-unsafe placeholder values for `BROKER_SHARED_SECRET`. The broker's boot
/// guard and per-request auth treat these as "unset" (fail-closed).
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
/// Serializes as the lowercase literals `persistent` / `ephemeral` (D12 — the
/// values honoured by `BROKER_DEFAULT_PROFILE` and the `X-Persistence`
/// header). Defaults to [`Profile::Persistent`], the broker deploy default.
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

/// Deploy-selectable backing for the **persistent** profile (env
/// `BROKER_PERSISTENT_MODE`, default `per-user-pvc`) — issue #140.
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
#[serde(rename_all = "kebab-case")]
pub enum PersistentMode {
    /// One PVC per user (`workspace-p-<sha256(user)[:12]>`, broker-created),
    /// each chat mounting its own `chats/<sha256(user/session)[:12]>` subPath.
    #[default]
    PerUserPvc,
    /// One shared RWX PVC (chart-rendered `workspace-shared`), each chat
    /// mounting `users/<sha256(user)[:12]>/chats/<sha256(user/session)[:12]>`.
    SharedSubpath,
    /// emptyDir hot tier + S3 cold tier (issue #52); requires
    /// `BROKER_S3_ENABLED`.
    S3Tiered,
}

impl PersistentMode {
    /// Whether this mode backs `/workspace` with a PVC the broker must
    /// ensure/mount (both PVC hot tiers; `false` for `s3-tiered`).
    #[must_use]
    pub fn is_pvc(self) -> bool {
        matches!(
            self,
            PersistentMode::PerUserPvc | PersistentMode::SharedSubpath
        )
    }

    /// Lowercase wire value (`per-user-pvc` / `shared-subpath` / `s3-tiered`),
    /// used for the `broker-persistent-mode` Sandbox label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PersistentMode::PerUserPvc => "per-user-pvc",
            PersistentMode::SharedSubpath => "shared-subpath",
            PersistentMode::S3Tiered => "s3-tiered",
        }
    }
}

impl FromStr for PersistentMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "per-user-pvc" => Ok(PersistentMode::PerUserPvc),
            "shared-subpath" => Ok(PersistentMode::SharedSubpath),
            "s3-tiered" => Ok(PersistentMode::S3Tiered),
            _ => Err(()),
        }
    }
}

/// Broker configuration loaded from the environment.
///
/// Field names mirror the env-var names the chart honours (D12 — drop-in).
/// Later PRs add the remaining knobs (warm-pool sizing, storage tiering, ...).
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

    /// Shared Bearer secret authenticating Open `WebUI` ↔ broker requests (env
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

    /// Backing for the persistent profile (env `BROKER_PERSISTENT_MODE`,
    /// default `per-user-pvc`). Unknown values fail boot (fail-closed) — a
    /// silently ignored mode is how #140's emptyDir data loss happened.
    #[serde(default)]
    pub persistent_mode: PersistentMode,

    /// Size of each per-user PVC in `per-user-pvc` mode (env
    /// `BROKER_PERSISTENT_STORAGE`, default `10Gi`).
    #[serde(default = "default_persistent_storage")]
    pub persistent_storage: String,

    /// StorageClass for per-user PVCs (env `BROKER_PERSISTENT_STORAGE_CLASS`).
    /// Empty ⇒ the cluster default StorageClass. Should be an RWX class
    /// (e.g. CephFS) in production — chat sandboxes of one user mount the
    /// same PVC concurrently.
    #[serde(default)]
    pub persistent_storage_class: String,

    /// Access modes for per-user PVCs (env `BROKER_PERSISTENT_ACCESS_MODES`,
    /// comma-separated; default `ReadWriteMany`). KIND/local-path deployments
    /// may set `ReadWriteOnce` (single-node clusters keep every chat pod of a
    /// user co-scheduled on one node).
    #[serde(default = "default_persistent_access_modes")]
    pub persistent_access_modes: Vec<String>,

    /// Name of the chart-rendered shared PVC used in `shared-subpath` mode
    /// (env `BROKER_SHARED_PVC`, default `workspace-shared`).
    #[serde(default = "default_shared_pvc_name")]
    pub shared_pvc_name: String,

    /// Prefix for per-user PVC names in `per-user-pvc` mode (env
    /// `BROKER_PER_USER_PVC_PREFIX`, default `workspace-p-`); the full name is
    /// `<prefix><sha256(user)[:12]>`.
    #[serde(default = "default_per_user_pvc_prefix")]
    pub per_user_pvc_prefix: String,

    /// Shared broker→runtime API key the broker injects as `Authorization:
    /// Bearer <key>` on the direct pod hop (env `BROKER_RUNTIME_API_KEY`).
    ///
    /// PR-C-2 uses this single shared key; PR-C-3 replaces it with the per-session
    /// `owui-runtime-key-<sandbox>` Secret the broker mints/rotates (issue #4).
    /// Empty ⇒ no Authorization header is forwarded (the runtime then fail-closes).
    #[serde(default)]
    pub runtime_api_key: String,

    /// Seconds to wait for a freshly created/resumed Sandbox to reach `Ready`
    /// (env `BROKER_CLAIM_TIMEOUT_SECONDS`, default `60`). Expiry surfaces as
    /// HTTP 503 (sandbox unavailable).
    #[serde(default = "default_claim_timeout_seconds")]
    pub claim_timeout_seconds: u64,

    /// Total seconds allowed for one broker→runtime pod hop (env
    /// `BROKER_PROXY_TIMEOUT_SECONDS`, default `660`); applied to the shared
    /// `reqwest` client.
    #[serde(default = "default_proxy_timeout_seconds")]
    pub proxy_timeout_seconds: u64,

    /// Idle seconds after which an **ephemeral** (emptyDir) Sandbox is reaped
    /// (env `BROKER_IDLE_TTL_SECONDS`, default `120`) — the time a session's
    /// sandbox stays warm with no activity before the reaper deletes it and
    /// returns its capacity to the pool.
    #[serde(default = "default_idle_ttl_seconds")]
    pub idle_ttl_seconds: u64,

    /// Idle seconds after which a **persistent** Sandbox is parked — pod deleted,
    /// node freed, `Sandbox` object + PVC retained for resume (env
    /// `BROKER_PARK_IDLE_SECONDS`, default `120`).
    #[serde(default = "default_park_idle_seconds")]
    pub park_idle_seconds: u64,

    /// Idle seconds after which a **persistent** Sandbox is fully reaped — the
    /// `Sandbox` object (and its released PVC claim) is deleted (env
    /// `BROKER_REAP_SECONDS`, default `604_800` = 7 days). Always greater than
    /// [`Self::park_idle_seconds`].
    #[serde(default = "default_reap_seconds")]
    pub reap_seconds: u64,

    /// Interval between idle-reaper sweeps (env `BROKER_REAPER_POLL_SECONDS`,
    /// default `60`). Only the elected leader runs the loop; non-leaders skip
    /// reaping entirely.
    #[serde(default = "default_reaper_poll_seconds")]
    pub reaper_poll_seconds: u64,

    /// Per-user rate limiting (env `BROKER_RATE_LIMIT_ENABLED`, default `true`).
    /// When enabled, a token-bucket per `X-User-Id` caps create / execute / file /
    /// terminal traffic on the broker's gated routes (`429` + `Retry-After` when the
    /// bucket is empty); open probes (`/healthz`, `/readyz`, `/metrics`) stay unlimited.
    #[serde(default = "default_rate_limit_enabled")]
    pub rate_limit_enabled: bool,

    /// Token-bucket refill rate, requests per second per user (env
    /// `BROKER_RATE_LIMIT_PER_SECOND`, default `20`).
    #[serde(default = "default_rate_limit_per_second")]
    pub rate_limit_per_second: u32,

    /// Token-bucket capacity / burst size per user (env
    /// `BROKER_RATE_LIMIT_BURST`, default `40`).
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    /// Namespace holding the broker leader-election `Lease` (env
    /// `BROKER_LEADER_NAMESPACE`). Defaults to [`Self::runtime_ns`] when unset.
    #[serde(default = "default_runtime_ns")]
    pub leader_namespace: String,

    /// Name of the `coordination.k8s.io/v1` `Lease` only the elected broker holds
    /// (env `BROKER_LEADER_LEASE`, default `owui-broker-leader`).
    #[serde(default = "default_leader_lease")]
    pub leader_lease: String,

    /// `Lease.spec.leaseDurationSeconds` — how long a holder's claim stays valid
    /// without a renew (env `BROKER_LEADER_DURATION_SECONDS`, default `15`). A
    /// holder whose `renewTime` is older than this is considered expired and
    /// another broker may take over.
    #[serde(default = "default_leader_duration_seconds")]
    pub leader_duration_seconds: u64,

    /// How often the leader loop renews (or re-attempts) the lease (env
    /// `BROKER_LEADER_RENEW_SECONDS`, default `5`); kept well under
    /// [`Self::leader_duration_seconds`] so a holder stays ahead of its own expiry.
    #[serde(default = "default_leader_renew_seconds")]
    pub leader_renew_seconds: u64,

    // --- S3 cold tier (issue #52; PR-C-4) ---------------------------------
    // The broker is the SOLE S3 client (#50): it streams a sandbox's /workspace
    // off to S3 on reap and back on resume. Fully behind `s3_enabled` (default
    // off); the real `aws-sdk-s3` client is only constructed when enabled. Env
    // env names follow the `BROKER_S3_*` convention (D12 drop-in).
    /// Gate the whole S3 cold tier (env `BROKER_S3_ENABLED`; `1`/`true`/`yes`/`on`).
    /// When false the reaper uses [`NoopOffload`](../../broker/reaper/struct.NoopOffload.html)
    /// and resolve skips restore (no cold tier).
    #[serde(default)]
    pub s3_enabled: bool,

    /// S3-compatible endpoint URL (env `BROKER_S3_ENDPOINT`). Empty ⇒ the AWS
    /// default (`https://s3.<region>.amazonaws.com`). Set for MinIO/R2/Proxmox
    /// (e.g. `http://minio:9000`).
    #[serde(default)]
    pub s3_endpoint: String,

    /// AWS region (env `BROKER_S3_REGION`, default `us-east-1`).
    #[serde(default = "default_s3_region")]
    pub s3_region: String,

    /// Bucket name (env `BROKER_S3_BUCKET`).
    #[serde(default)]
    pub s3_bucket: String,

    /// Object-key prefix (env `BROKER_S3_PREFIX`, default `users`); leading/
    /// trailing slashes are stripped. Each sandbox's snapshots live under
    /// `<prefix>/<sandbox>/` (the namespace).
    #[serde(default = "default_s3_prefix")]
    pub s3_prefix: String,

    /// Static access key id (env `BROKER_S3_ACCESS_KEY_ID`). Empty ⇒ rely on
    /// the SDK's default credential chain.
    #[serde(default)]
    pub s3_access_key_id: String,

    /// Static secret access key (env `BROKER_S3_SECRET_ACCESS_KEY`).
    #[serde(default)]
    pub s3_secret_access_key: String,

    /// Force path-style addressing (`<endpoint>/<bucket>/<key>`) — required by
    /// MinIO/R2/Proxmox and works on AWS S3 too (env `BROKER_S3_PATH_STYLE`,
    /// default `true`).
    #[serde(default = "default_s3_path_style")]
    pub s3_path_style: bool,
    /// Server-side encryption mode (env `BROKER_S3_SSE`; e.g. `"AES256"` for
    /// SSE-S3). Empty or `"none"` disables SSE — required for stores without a
    /// KMS/SSE backend (dev `MinIO`) (D12).
    #[serde(default)]
    pub s3_sse: String,
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

const fn default_idle_ttl_seconds() -> u64 {
    120
}

const fn default_park_idle_seconds() -> u64 {
    120
}

const fn default_reap_seconds() -> u64 {
    7 * 24 * 3600 // 7 days
}

const fn default_reaper_poll_seconds() -> u64 {
    60
}

/// Default: rate limiting enabled (#98 A3).
const fn default_rate_limit_enabled() -> bool {
    true
}
/// Default: 20 requests/sec/user (#98 A3).
const fn default_rate_limit_per_second() -> u32 {
    20
}
/// Default: burst of 40/user (#98 A3).
const fn default_rate_limit_burst() -> u32 {
    40
}

fn default_leader_lease() -> String {
    "owui-broker-leader".to_string()
}

const fn default_leader_duration_seconds() -> u64 {
    15
}

fn default_leader_renew_seconds() -> u64 {
    5
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
}

fn default_s3_prefix() -> String {
    "users".to_string()
}

const fn default_s3_path_style() -> bool {
    true
}

fn default_persistent_storage() -> String {
    "10Gi".to_string()
}

fn default_persistent_access_modes() -> Vec<String> {
    vec!["ReadWriteMany".to_string()]
}

fn default_shared_pvc_name() -> String {
    "workspace-shared".to_string()
}

fn default_per_user_pvc_prefix() -> String {
    "workspace-p-".to_string()
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
            persistent_mode: PersistentMode::PerUserPvc,
            persistent_storage: default_persistent_storage(),
            persistent_storage_class: String::new(),
            persistent_access_modes: default_persistent_access_modes(),
            shared_pvc_name: default_shared_pvc_name(),
            per_user_pvc_prefix: default_per_user_pvc_prefix(),
            runtime_api_key: String::new(),
            claim_timeout_seconds: default_claim_timeout_seconds(),
            proxy_timeout_seconds: default_proxy_timeout_seconds(),
            idle_ttl_seconds: default_idle_ttl_seconds(),
            park_idle_seconds: default_park_idle_seconds(),
            reap_seconds: default_reap_seconds(),
            reaper_poll_seconds: default_reaper_poll_seconds(),
            rate_limit_enabled: default_rate_limit_enabled(),
            rate_limit_per_second: default_rate_limit_per_second(),
            rate_limit_burst: default_rate_limit_burst(),
            leader_namespace: default_runtime_ns(),
            leader_lease: default_leader_lease(),
            leader_duration_seconds: default_leader_duration_seconds(),
            leader_renew_seconds: default_leader_renew_seconds(),
            s3_enabled: false,
            s3_endpoint: String::new(),
            s3_region: default_s3_region(),
            s3_bucket: String::new(),
            s3_prefix: default_s3_prefix(),
            s3_access_key_id: String::new(),
            s3_secret_access_key: String::new(),
            s3_path_style: default_s3_path_style(),
            s3_sse: String::new(),
        }
    }
}

impl BrokerConfig {
    /// Load configuration from process environment variables, applying the
    /// documented defaults (D12).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] when a recognised numeric variable is
    /// set to a value that cannot be parsed; absent variables fall back to
    /// their documented defaults. A malformed `BROKER_DEFAULT_PROFILE` falls
    /// back to `persistent` (a defensive fallback) rather than
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
        let runtime_ns = get("BROKER_RUNTIME_NS").unwrap_or_else(default_runtime_ns);
        // BROKER_LEADER_NAMESPACE defaults to the resolved runtime_ns.
        let leader_namespace = get("BROKER_LEADER_NAMESPACE").unwrap_or_else(|| runtime_ns.clone());
        // S3 static credentials: prefer explicit env (BROKER_S3_ACCESS_KEY_ID/_SECRET),
        // else read the projected Secret volume at $BROKER_S3_CREDS_DIR (default
        // /etc/s3-creds) the Helm chart mounts (#48: no secret in env). Empty
        // => SDK default credential chain.
        let (s3_access_key_id, s3_secret_access_key) = resolve_s3_creds(
            &get,
            &get("BROKER_S3_ACCESS_KEY_ID").unwrap_or_default(),
            &get("BROKER_S3_SECRET_ACCESS_KEY").unwrap_or_default(),
        );
        let cfg = Self {
            max_terminal_sessions: env_value("MAX_TERMINAL_SESSIONS", &get)?
                .unwrap_or_else(default_max_terminal_sessions),
            max_output_bytes: env_value("MAX_OUTPUT_BYTES", &get)?
                .unwrap_or_else(default_max_output_bytes),
            shared_secret: get("BROKER_SHARED_SECRET").unwrap_or_default(),
            runtime_ns,
            base_template: get("BROKER_BASE_TEMPLATE").unwrap_or_else(default_base_template),
            default_profile: get("BROKER_DEFAULT_PROFILE")
                .and_then(|raw| raw.parse().ok())
                .unwrap_or_default(),
            persistent_mode: get("BROKER_PERSISTENT_MODE")
                .map(|raw| {
                    raw.parse::<PersistentMode>()
                        .map_err(|()| ConfigError::Invalid {
                            var: "BROKER_PERSISTENT_MODE",
                            message: format!(
                                "{raw:?} is not one of per-user-pvc | shared-subpath | s3-tiered",
                            ),
                        })
                })
                .transpose()?
                .unwrap_or_default(),
            persistent_storage: get("BROKER_PERSISTENT_STORAGE")
                .unwrap_or_else(default_persistent_storage),
            persistent_storage_class: get("BROKER_PERSISTENT_STORAGE_CLASS").unwrap_or_default(),
            persistent_access_modes: get("BROKER_PERSISTENT_ACCESS_MODES")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(default_persistent_access_modes),
            shared_pvc_name: get("BROKER_SHARED_PVC").unwrap_or_else(default_shared_pvc_name),
            per_user_pvc_prefix: get("BROKER_PER_USER_PVC_PREFIX")
                .unwrap_or_else(default_per_user_pvc_prefix),
            runtime_api_key: get("BROKER_RUNTIME_API_KEY").unwrap_or_default(),
            claim_timeout_seconds: env_value("BROKER_CLAIM_TIMEOUT_SECONDS", &get)?
                .unwrap_or_else(default_claim_timeout_seconds),
            proxy_timeout_seconds: env_value("BROKER_PROXY_TIMEOUT_SECONDS", &get)?
                .unwrap_or_else(default_proxy_timeout_seconds),
            idle_ttl_seconds: env_value("BROKER_IDLE_TTL_SECONDS", &get)?
                .unwrap_or_else(default_idle_ttl_seconds),
            park_idle_seconds: env_value("BROKER_PARK_IDLE_SECONDS", &get)?
                .unwrap_or_else(default_park_idle_seconds),
            reap_seconds: env_value("BROKER_REAP_SECONDS", &get)?
                .unwrap_or_else(default_reap_seconds),
            reaper_poll_seconds: env_value("BROKER_REAPER_POLL_SECONDS", &get)?
                .unwrap_or_else(default_reaper_poll_seconds),
            rate_limit_enabled: get("BROKER_RATE_LIMIT_ENABLED").is_none_or(|raw| parse_bool(&raw)),
            rate_limit_per_second: env_value("BROKER_RATE_LIMIT_PER_SECOND", &get)?
                .unwrap_or_else(default_rate_limit_per_second),
            rate_limit_burst: env_value("BROKER_RATE_LIMIT_BURST", &get)?
                .unwrap_or_else(default_rate_limit_burst),
            leader_namespace,
            leader_lease: get("BROKER_LEADER_LEASE").unwrap_or_else(default_leader_lease),
            leader_duration_seconds: env_value("BROKER_LEADER_DURATION_SECONDS", &get)?
                .unwrap_or_else(default_leader_duration_seconds),
            leader_renew_seconds: env_value("BROKER_LEADER_RENEW_SECONDS", &get)?
                .unwrap_or_else(default_leader_renew_seconds),
            s3_enabled: get("BROKER_S3_ENABLED").is_some_and(|raw| parse_bool(&raw)),
            s3_endpoint: get("BROKER_S3_ENDPOINT").unwrap_or_default(),
            s3_region: get("BROKER_S3_REGION").unwrap_or_else(default_s3_region),
            s3_bucket: get("BROKER_S3_BUCKET").unwrap_or_default(),
            s3_prefix: get("BROKER_S3_PREFIX")
                .map_or_else(default_s3_prefix, |raw| trim_prefix(&raw)),
            s3_sse: get("BROKER_S3_SSE").unwrap_or_default(),
            s3_access_key_id,
            s3_secret_access_key,
            s3_path_style: get("BROKER_S3_PATH_STYLE").is_none_or(|raw| parse_bool(&raw)),
        };
        // Mode exclusivity (#140): the S3 cold tier is wired ONLY into the
        // s3-tiered mode (offload-on-reap + restore-on-resume would fight a
        // PVC-backed workspace), and s3-tiered without S3 configured cannot
        // keep any promise of persistence. Fail closed at boot either way.
        match (cfg.persistent_mode, cfg.s3_enabled) {
            (PersistentMode::S3Tiered, false) => {
                return Err(ConfigError::Invalid {
                    var: "BROKER_PERSISTENT_MODE",
                    message: "s3-tiered requires broker.s3.enabled=true (BROKER_S3_ENABLED)"
                        .to_string(),
                });
            }
            (mode, true) if !matches!(mode, PersistentMode::S3Tiered) => {
                return Err(ConfigError::Invalid {
                    var: "BROKER_PERSISTENT_MODE",
                    message: format!(
                        "broker.s3.enabled=true requires BROKER_PERSISTENT_MODE=s3-tiered (got {mode:?})",
                    ),
                });
            }
            _ => {}
        }
        Ok(cfg)
    }
}

/// Resolve S3 static credentials for [`BrokerConfig::from_map`].
///
/// Prefers explicit env vars (`BROKER_S3_ACCESS_KEY_ID` / `_SECRET`); when the access
/// key id is absent, reads the projected Secret volume the Helm chart mounts at
/// `$BROKER_S3_CREDS_DIR` (default `/etc/s3-creds`) — the `access-key-id` and
/// `secret-access-key` files — so the broker authenticates without a secret in env
/// (#48). Returns `("", "")` when neither source is
/// set, leaving authentication to the SDK default chain (env, IMDS, …).
fn resolve_s3_creds<G>(get: &G, env_access: &str, env_secret: &str) -> (String, String)
where
    G: Fn(&str) -> Option<String>,
{
    if !env_access.is_empty() {
        return (env_access.to_string(), env_secret.to_string());
    }
    let dir = get("BROKER_S3_CREDS_DIR").unwrap_or_else(|| "/etc/s3-creds".to_string());
    let read = |name: &str| {
        std::fs::read_to_string(format!("{dir}/{name}"))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    (read("access-key-id"), read("secret-access-key"))
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

/// Parse a boolean: case-insensitive `1`/`true`/`yes`/`on` ⇒ `true`,
/// anything else ⇒ `false` (used for S3/PROFILE toggles).
fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Strip leading/trailing `/` from an S3 prefix segment; collapses the
/// object-key namespace so `<prefix>/<sandbox>/` is canonical regardless of
/// trailing slashes in env.
fn trim_prefix(raw: &str) -> String {
    raw.trim_matches('/').to_string()
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
    fn serde_defaults_cover_reaper_and_leader_fields() {
        let cfg: BrokerConfig = serde_json::from_str("{}").expect("empty object");
        // Reaper TTLs: IDLE_TTL / PARK_TTL / REAP_TTL / REAPER_POLL.
        assert_eq!(cfg.idle_ttl_seconds, 120);
        assert_eq!(cfg.park_idle_seconds, 120);
        assert_eq!(cfg.reap_seconds, 7 * 24 * 3600);
        assert_eq!(cfg.reaper_poll_seconds, 60);
        // Leader-lease params: _LEADER_*.
        assert_eq!(cfg.leader_namespace, "agent-sandbox-runtime");
        assert_eq!(cfg.leader_lease, "owui-broker-leader");
        assert_eq!(cfg.leader_duration_seconds, 15);
        assert_eq!(cfg.leader_renew_seconds, 5);
    }

    #[test]
    fn serde_defaults_cover_s3_fields() {
        let cfg: BrokerConfig = serde_json::from_str("{}").expect("empty object");
        // S3 cold tier defaults off; path-style on (hard-coded
        // `addressing_style: "path"`); prefix defaults to `users`.
        assert!(!cfg.s3_enabled);
        assert_eq!(cfg.s3_endpoint, "");
        assert_eq!(cfg.s3_region, "us-east-1");
        assert_eq!(cfg.s3_bucket, "");
        assert_eq!(cfg.s3_prefix, "users");
        assert_eq!(cfg.s3_access_key_id, "");
        assert_eq!(cfg.s3_secret_access_key, "");
        assert!(cfg.s3_path_style);
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
            idle_ttl_seconds: 90,
            park_idle_seconds: 91,
            reap_seconds: 99_999,
            reaper_poll_seconds: 5,
            leader_namespace: "lead-ns".into(),
            leader_lease: "custom-lease".into(),
            leader_duration_seconds: 30,
            leader_renew_seconds: 10,
            s3_enabled: true,
            s3_endpoint: "http://minio:9000".into(),
            s3_region: "us-east-1".into(),
            s3_bucket: "owui-cold".into(),
            s3_prefix: "users".into(),
            s3_access_key_id: "AKIAEXAMPLE".into(),
            s3_secret_access_key: "secret".into(),
            s3_path_style: true,
            s3_sse: "AES256".into(),
            rate_limit_enabled: true,
            rate_limit_per_second: 7,
            rate_limit_burst: 14,
            persistent_mode: PersistentMode::S3Tiered,
            persistent_storage: "5Gi".into(),
            persistent_storage_class: "cephfs".into(),
            persistent_access_modes: vec!["ReadWriteMany".into()],
            shared_pvc_name: "shared-ws".into(),
            per_user_pvc_prefix: "ws-p-".into(),
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        assert!(json.contains("\"max_terminal_sessions\":2"), "{json}");
        assert!(json.contains("\"default_profile\":\"ephemeral\""), "{json}");
        assert!(json.contains("\"runtime_api_key\":\"rt-key\""), "{json}");
        assert!(json.contains("\"claim_timeout_seconds\":30"), "{json}");
        assert!(json.contains("\"s3_enabled\":true"), "{json}");
        assert!(json.contains("\"s3_bucket\":\"owui-cold\""), "{json}");
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
            ("BROKER_IDLE_TTL_SECONDS", "7"),
            ("BROKER_PARK_IDLE_SECONDS", "8"),
            ("BROKER_REAP_SECONDS", "9"),
            ("BROKER_REAPER_POLL_SECONDS", "1"),
            ("BROKER_LEADER_NAMESPACE", "lead-ns"),
            ("BROKER_LEADER_LEASE", "custom-lease"),
            ("BROKER_LEADER_DURATION_SECONDS", "30"),
            ("BROKER_LEADER_RENEW_SECONDS", "10"),
            ("BROKER_S3_ENABLED", "yes"),
            ("BROKER_PERSISTENT_MODE", "s3-tiered"),
            ("BROKER_S3_ENDPOINT", "http://minio:9000"),
            ("BROKER_S3_REGION", "eu-west-1"),
            ("BROKER_S3_BUCKET", "owui-cold"),
            ("BROKER_S3_PREFIX", "//prod/users//"),
            ("BROKER_S3_ACCESS_KEY_ID", "AKIAEXAMPLE"),
            ("BROKER_S3_SECRET_ACCESS_KEY", "shh"),
            ("BROKER_S3_PATH_STYLE", "off"),
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
        assert_eq!(cfg.idle_ttl_seconds, 7);
        assert_eq!(cfg.park_idle_seconds, 8);
        assert_eq!(cfg.reap_seconds, 9);
        assert_eq!(cfg.reaper_poll_seconds, 1);
        assert_eq!(cfg.leader_namespace, "lead-ns");
        assert_eq!(cfg.leader_lease, "custom-lease");
        assert_eq!(cfg.leader_duration_seconds, 30);
        assert_eq!(cfg.leader_renew_seconds, 10);
        assert!(cfg.s3_enabled, "BROKER_S3_ENABLED=yes => true");
        assert_eq!(cfg.s3_endpoint, "http://minio:9000");
        assert_eq!(cfg.s3_region, "eu-west-1");
        assert_eq!(cfg.s3_bucket, "owui-cold");
        assert_eq!(cfg.s3_prefix, "prod/users", "surrounding slashes stripped");
        assert_eq!(cfg.s3_access_key_id, "AKIAEXAMPLE");
        assert_eq!(cfg.s3_secret_access_key, "shh");
        assert!(!cfg.s3_path_style, "BROKER_S3_PATH_STYLE=off => false");
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
        assert_eq!(cfg.idle_ttl_seconds, 120);
        assert_eq!(cfg.park_idle_seconds, 120);
        assert_eq!(cfg.reap_seconds, 7 * 24 * 3600);
        assert_eq!(cfg.reaper_poll_seconds, 60);
        // Leader namespace defaults to runtime_ns when unset.
        assert_eq!(cfg.leader_namespace, cfg.runtime_ns);
        assert_eq!(cfg.leader_lease, "owui-broker-leader");
        assert_eq!(cfg.leader_duration_seconds, 15);
        assert_eq!(cfg.leader_renew_seconds, 5);
        // S3 cold tier defaults: disabled, empty endpoint/creds, AWS default
        // region, `users` prefix, path-style on (hard-coded
        // addressing_style="path").
        assert!(!cfg.s3_enabled);
        assert_eq!(cfg.s3_endpoint, "");
        assert_eq!(cfg.s3_region, "us-east-1");
        assert_eq!(cfg.s3_bucket, "");
        assert_eq!(cfg.s3_prefix, "users");
        assert!(cfg.s3_path_style);
    }

    #[test]
    fn leader_namespace_defaults_to_runtime_ns_when_overridden() {
        // When the runtime namespace is overridden but the leader namespace is
        // left unset, the leader namespace follows the override.
        let cfg = BrokerConfig::from_map(map(&[("BROKER_RUNTIME_NS", "custom-rt")])).expect("ok");
        assert_eq!(cfg.runtime_ns, "custom-rt");
        assert_eq!(cfg.leader_namespace, "custom-rt");
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

    #[test]
    fn s3_creds_read_from_files_when_env_absent() {
        // The Helm chart projects the Secret at /etc/s3-creds as files (#48: no
        // secret in env). When the env creds are absent the broker must read them
        // from there.
        let dir = std::env::temp_dir().join(format!("owsb-s3-creds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("access-key-id"), "  file-akid  \n").unwrap();
        std::fs::write(dir.join("secret-access-key"), "file-secret\n").unwrap();
        let dir_str = dir.to_string_lossy().into_owned();
        let get = |k: &str| match k {
            "BROKER_S3_CREDS_DIR" => Some(dir_str.clone()),
            _ => None,
        };
        // Empty env => fall back to the projected files (whitespace trimmed).
        let (ak, sk) = resolve_s3_creds(&get, "", "");
        assert_eq!(ak, "file-akid");
        assert_eq!(sk, "file-secret");
        // Explicit env always wins over the files.
        let (ak2, sk2) = resolve_s3_creds(&get, "env-akid", "env-secret");
        assert_eq!((ak2.as_str(), sk2.as_str()), ("env-akid", "env-secret"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn s3_sse_read_from_env() {
        let cfg = BrokerConfig::from_map(|k: &str| match k {
            "BROKER_S3_ENABLED" => Some("true".to_string()),
            "BROKER_PERSISTENT_MODE" => Some("s3-tiered".to_string()),
            "BROKER_S3_SSE" => Some("AES256".to_string()),
            "BROKER_S3_BUCKET" => Some("b".to_string()),
            _ => None,
        })
        .expect("config");
        assert_eq!(cfg.s3_sse, "AES256");
        assert!(cfg.s3_enabled);
    }

    #[test]
    fn persistent_mode_defaults_and_parses() {
        let cfg = BrokerConfig::from_map(|_| None).expect("config");
        assert_eq!(cfg.persistent_mode, PersistentMode::PerUserPvc);
        assert_eq!(cfg.persistent_storage, "10Gi");
        assert_eq!(cfg.persistent_storage_class, "");
        assert_eq!(cfg.persistent_access_modes, ["ReadWriteMany".to_string()]);
        assert_eq!(cfg.shared_pvc_name, "workspace-shared");
        assert_eq!(cfg.per_user_pvc_prefix, "workspace-p-");

        let cfg = BrokerConfig::from_map(|k| match k {
            "BROKER_PERSISTENT_MODE" => Some("shared-subpath".to_string()),
            "BROKER_PERSISTENT_STORAGE" => Some("5Gi".to_string()),
            "BROKER_PERSISTENT_STORAGE_CLASS" => Some("cephfs".to_string()),
            "BROKER_PERSISTENT_ACCESS_MODES" => Some(" ReadWriteOnce ,  ".to_string()),
            "BROKER_SHARED_PVC" => Some("ws".to_string()),
            "BROKER_PER_USER_PVC_PREFIX" => Some("p-".to_string()),
            _ => None,
        })
        .expect("config");
        assert_eq!(cfg.persistent_mode, PersistentMode::SharedSubpath);
        assert!(cfg.persistent_mode.is_pvc());
        assert_eq!(cfg.persistent_storage, "5Gi");
        assert_eq!(cfg.persistent_storage_class, "cephfs");
        assert_eq!(cfg.persistent_access_modes, ["ReadWriteOnce".to_string()]);
        assert_eq!(cfg.shared_pvc_name, "ws");
        assert_eq!(cfg.per_user_pvc_prefix, "p-");
    }

    #[test]
    fn unknown_persistent_mode_fails_closed() {
        let err = BrokerConfig::from_map(|k| {
            (k == "BROKER_PERSISTENT_MODE").then(|| "nonsense".to_string())
        })
        .expect_err("unknown mode must fail boot");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                var: "BROKER_PERSISTENT_MODE",
                ..
            }
        ));
    }

    #[test]
    fn s3_mode_exclusivity_fails_closed() {
        // s3-tiered without S3 configured.
        let err = BrokerConfig::from_map(|k| {
            (k == "BROKER_PERSISTENT_MODE").then(|| "s3-tiered".to_string())
        })
        .expect_err("s3-tiered without S3 must fail boot");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                var: "BROKER_PERSISTENT_MODE",
                ..
            }
        ));

        // S3 enabled with a PVC mode.
        let err = BrokerConfig::from_map(|k| match k {
            "BROKER_S3_ENABLED" => Some("true".to_string()),
            "BROKER_PERSISTENT_MODE" => Some("per-user-pvc".to_string()),
            _ => None,
        })
        .expect_err("S3 with a PVC mode must fail boot");
        assert!(matches!(
            err,
            ConfigError::Invalid {
                var: "BROKER_PERSISTENT_MODE",
                ..
            }
        ));
    }
}
