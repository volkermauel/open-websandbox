//! Fail-closed per-session API-key authentication (issues #4 / #50).
//!
//! Direct port of the Python `server` key handling. Each sandbox pod gets its
//! OWN broker↔runtime key, delivered as a projected Secret volume mounted at
//! `RUNTIME_KEY_FILE` (`/etc/runtime-key/api-key`). The runtime reads it from
//! that FILE (never the env), mtime-caching the value so a rotated Secret —
//! re-synced by the kubelet with a fresh mtime — is picked up without a restart
//! (rotate-on-resume). Comparison is constant-time via
//! [`shared::constant_time_eq`].
//!
//! Fail-closed contract:
//! * an absent/empty/placeholder key file is a misconfiguration → 503 at the
//!   request path AND a refused boot ([`SessionKeyStore::validate`]);
//! * a missing or mismatched Bearer → 401.
//!
//! The 9 cases in `tests/unit/runtime/test_runtime_auth.py` are ported in
//! `tests/auth_contract.rs`.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;

use shared::constant_time_eq;

use crate::error::ApiError;
use crate::state::AppState;

/// Keys that count as "not configured" (must not be shipped as-is). Matches the
/// Python `_PLACEHOLDER_KEYS` frozenset exactly.
const PLACEHOLDER_KEYS: &[&str] = &[
    "",
    "dev-shared-secret-change-me",
    "change-me",
    "changeme",
    "placeholder",
];

/// Sentinel "the file is missing" cache state (Python uses mtime `-2.0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mtime {
    /// `RUNTIME_KEY_FILE` does not exist / is unreadable.
    Missing,
    /// The file exists with this `mtime`.
    Live(SystemTime),
}

#[derive(Debug, Default)]
struct KeyCache {
    /// `None` = invalidated (force re-read on next [`SessionKeyStore::load`]).
    mtime: Option<Mtime>,
    value: String,
}

/// Mtime-cached reader for the projected-Secret per-session key.
///
/// Thread-safe via an internal mutex; the value is small and rarely changes, so
/// the lock is held only for the duration of a `stat`/`read`.
pub struct SessionKeyStore {
    key_file: PathBuf,
    cache: Mutex<KeyCache>,
}

/// Outcome of authenticating one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Bearer matched the configured key.
    Ok,
    /// Key file absent/empty/placeholder — fail-closed 503.
    Unconfigured,
    /// Bearer missing or did not match (even after the rotate-on-resume reload).
    Invalid,
}

/// Boot-time failure: the per-session key is missing or a placeholder.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BootError(pub String);

fn is_placeholder(key: &str) -> bool {
    PLACEHOLDER_KEYS.contains(&key)
}

impl SessionKeyStore {
    /// Build a store backed by `key_file`.
    pub fn new(key_file: impl Into<PathBuf>) -> Self {
        Self {
            key_file: key_file.into(),
            cache: Mutex::new(KeyCache::default()),
        }
    }

    /// Read the per-session key (mtime-cached, fail-closed).
    ///
    /// Returns `""` when the file is absent/empty/unreadable. The mtime is
    /// cached so re-reads are cheap, and a rotated file (new mtime) is reflected
    /// without a restart.
    pub fn load(&self) -> String {
        let mut cache = self.cache.lock().expect("key cache mutex poisoned");
        let mtime = match fs::metadata(&self.key_file).and_then(|m| m.modified()) {
            Ok(t) => Mtime::Live(t),
            Err(_) => Mtime::Missing,
        };
        if cache_mtime_eq(cache.mtime, mtime) {
            return cache.value.clone();
        }
        // Cache miss / invalidated / changed: re-read.
        cache.value = match mtime {
            Mtime::Missing => String::new(),
            Mtime::Live(_) => fs::read_to_string(&self.key_file)
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        cache.mtime = Some(mtime);
        cache.value.clone()
    }

    /// Force the next [`SessionKeyStore::load`] to re-stat/re-read. Used on a
    /// mismatch so rotate-on-resume is honoured on the very next request.
    pub fn invalidate(&self) {
        let mut cache = self.cache.lock().expect("key cache mutex poisoned");
        cache.mtime = None;
    }

    /// Fail-closed boot guard: refuse to start with a missing/placeholder key.
    pub fn validate(&self) -> Result<(), BootError> {
        let key = self.load();
        if is_placeholder(&key) {
            return Err(BootError(format!(
                "per-session runtime API key is missing or a placeholder — refusing to start. \
                 The broker must inject a per-session Secret as the projected volume at {} \
                 (volume 'runtime-key').",
                self.key_file.display()
            )));
        }
        Ok(())
    }

    /// Authenticate one request (constant-time, reload-on-mismatch).
    pub fn check(&self, presented: Option<&[u8]>) -> AuthOutcome {
        let key = self.load();
        if is_placeholder(&key) {
            return AuthOutcome::Unconfigured;
        }
        if let Some(token) = presented {
            if constant_time_eq(token, key.as_bytes()) {
                return AuthOutcome::Ok;
            }
        }
        // Reload once: a just-rotated key may not yet be cached.
        self.invalidate();
        let key2 = self.load();
        if !is_placeholder(&key2) && presented.is_some_and(|t| constant_time_eq(t, key2.as_bytes()))
        {
            return AuthOutcome::Ok;
        }
        AuthOutcome::Invalid
    }
}

/// Compare the cached mtime sentinel with the freshly-observed one.
fn cache_mtime_eq(cached: Option<Mtime>, observed: Mtime) -> bool {
    match (cached, observed) {
        (Some(Mtime::Missing), Mtime::Missing) => true,
        (Some(Mtime::Live(a)), Mtime::Live(b)) => a == b,
        _ => false,
    }
}

/// Extract a `Bearer <token>` value from request headers (None if absent or
/// not a Bearer scheme).
pub fn bearer_from_headers(headers: &HeaderMap) -> Option<Vec<u8>> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = raw.strip_prefix("Bearer ")?;
    Some(token.as_bytes().to_vec())
}

/// Extractor proof that a request passed [`SessionKeyStore::check`].
///
/// Add this as the first handler parameter on every gated route; open routes
/// simply omit it. This mirrors the Python `Security(_auth_runtime)` dependency
/// wired onto each gated endpoint.
#[derive(Debug, Clone, Copy)]
pub struct Authed;

impl FromRequestParts<AppState> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match state
            .key_store
            .check(bearer_from_headers(&parts.headers).as_deref())
        {
            AuthOutcome::Ok => Ok(Authed),
            AuthOutcome::Unconfigured => Err(ApiError::ServiceUnavailable(
                "per-session runtime API key is not configured".to_string(),
            )),
            AuthOutcome::Invalid => Err(ApiError::Unauthorized(
                "invalid runtime api key".to_string(),
            )),
        }
    }
}

/// Convenience for tests / boot diagnostics.
#[allow(dead_code)]
pub(crate) fn placeholder_keys() -> &'static [&'static str] {
    PLACEHOLDER_KEYS
}

#[allow(dead_code)]
pub(crate) fn key_file_of(store: &SessionKeyStore) -> &Path {
    &store.key_file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_key() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("k-{}", unique()));
        p
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }

    fn write_key(path: &Path, contents: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn placeholder_detection() {
        for p in PLACEHOLDER_KEYS {
            assert!(is_placeholder(p), "{p:?} should be placeholder");
        }
        assert!(!is_placeholder(
            "a-very-strong-and-random-runtime-key-123456"
        ));
    }

    #[test]
    fn validate_rejects_placeholders() {
        for p in PLACEHOLDER_KEYS {
            let path = tmp_key();
            write_key(&path, p);
            let store = SessionKeyStore::new(&path);
            assert!(
                store.validate().is_err(),
                "{p:?} should be rejected at boot"
            );
        }
    }

    #[test]
    fn validate_rejects_missing_file() {
        let store = SessionKeyStore::new(tmp_key()); // never written
        assert!(store.validate().is_err());
    }

    #[test]
    fn validate_accepts_strong_key() {
        let path = tmp_key();
        write_key(&path, "a-very-strong-and-random-runtime-key-123456");
        let store = SessionKeyStore::new(&path);
        store.validate().expect("strong key should boot");
    }

    #[test]
    fn check_missing_bearer_is_invalid() {
        let path = tmp_key();
        write_key(&path, "s3cret-key");
        let store = SessionKeyStore::new(&path);
        assert_eq!(store.check(None), AuthOutcome::Invalid);
    }

    #[test]
    fn check_wrong_bearer_is_invalid() {
        let path = tmp_key();
        write_key(&path, "s3cret-key");
        let store = SessionKeyStore::new(&path);
        assert_eq!(store.check(Some(b"nope")), AuthOutcome::Invalid);
    }

    #[test]
    fn check_correct_bearer_is_ok() {
        let path = tmp_key();
        write_key(&path, "s3cret-key");
        let store = SessionKeyStore::new(&path);
        assert_eq!(store.check(Some(b"s3cret-key")), AuthOutcome::Ok);
    }

    #[test]
    fn check_unconfigured_is_503() {
        let store = SessionKeyStore::new(tmp_key()); // missing file
        assert_eq!(store.check(Some(b"anything")), AuthOutcome::Unconfigured);
        assert_eq!(store.check(None), AuthOutcome::Unconfigured);
    }

    #[test]
    fn reload_on_rotate_honours_new_key() {
        let path = tmp_key();
        write_key(&path, "old-key");
        let store = SessionKeyStore::new(&path);
        // Seed the cache with the old value.
        assert_eq!(store.check(Some(b"old-key")), AuthOutcome::Ok);
        // Rotate: overwrite the file. Give it a newer mtime so the cache misses.
        std::thread::sleep(std::time::Duration::from_millis(15));
        write_key(&path, "new-key");
        // Old key now rejected ...
        assert_eq!(store.check(Some(b"old-key")), AuthOutcome::Invalid);
        // ... freshly rotated key accepted on the next request.
        assert_eq!(store.check(Some(b"new-key")), AuthOutcome::Ok);
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer_from_headers(&h), None);
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer abc".parse().unwrap(),
        );
        assert_eq!(bearer_from_headers(&h), Some(b"abc".to_vec()));
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic xyz".parse().unwrap(),
        );
        assert_eq!(bearer_from_headers(&h), None);
    }
}
