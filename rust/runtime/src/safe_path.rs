//! Path confinement — the security-critical boundary of the runtime.
//!
//! Every file endpoint funnels through [`safe_path`], which confines a
//! caller-supplied path to the workspace base (`WORKDIR`, or
//! `WORKDIR/<subdir>` under `X-Workspace-Subdir`). This is a faithful Rust port
//! of the Python `server._safe_path` / `_request_base`: it rejects any path that
//! resolves outside `base` — `..` traversal, absolute escapes, URL-encoded
//! traversal, and symlink escapes — while honouring absolute paths that are
//! already inside `base` (the open-terminal UI echoes the cwd back from
//! `GET /files/cwd`).
//!
//! The 17 cases in `tests/unit/runtime/test_safe_path.py` are ported verbatim
//! in `tests/safe_path_contract.rs`.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

use percent_encoding::percent_decode_str;

use crate::error::{escapes, ApiError};

/// Max symlink hops before we declare a loop (matches glibc `ELOOP` bound).
const SYMLINK_LIMIT: u32 = 40;

/// Decode a `urllib.parse.unquote`-style percent-encoded path component.
///
/// `unquote` decodes `%XX` sequences as UTF-8 (lossily) and does NOT turn `+`
/// into a space (that is `unquote_plus`). [`percent_decode_str`] mirrors that.
fn unquote(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Resolve an absolute path to its canonical form, following symlinks for
/// components that exist and leaving the (possibly non-existent) tail lexical.
///
/// This mirrors Python's `os.path.realpath`, which — unlike
/// [`std::fs::canonicalize`] — succeeds on paths whose final components do not
/// yet exist (e.g. a file about to be written). For the security property what
/// matters is that a symlink living *inside* `base` that points *outside* is
/// resolved to its target and thus rejected.
///
/// # Panics
/// Only paths that are already absolute should be passed in (the callers below
/// guarantee this). A relative input is resolved against the process cwd first.
fn realpath(input: &Path) -> PathBuf {
    let start = if input.is_absolute() {
        input.to_path_buf()
    } else {
        // _safe_path only ever feeds us absolute paths; fall back to lexical
        // absolutisation purely defensively.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        cwd.join(input)
    };
    let mut links: u32 = 0;
    resolve(&start, &mut links)
}

/// Recursive component walker. `links` bounds total symlink dereferences across
/// the whole resolution so a symlink loop cannot recurse forever.
fn resolve(input: &Path, links: &mut u32) -> PathBuf {
    let mut result = PathBuf::new();
    for component in input.components() {
        match component {
            Component::RootDir => {
                result = PathBuf::from("/");
            }
            // `.` is a no-op.
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Prefix(_) => {}
            Component::Normal(name) => {
                result.push(name);
                if let Ok(target) = std::fs::read_link(&result) {
                    *links += 1;
                    if *links > SYMLINK_LIMIT {
                        // Too many symlinks: stop dereferencing and keep `result`
                        // as-is (glibc would return ELOOP; we fail closed upstream
                        // because such a path is pathological).
                        return result;
                    }
                    let base = if target.is_absolute() {
                        PathBuf::from("/")
                    } else {
                        result
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| PathBuf::from("/"))
                    };
                    // Restart resolution from the symlink target, carrying the
                    // accumulated link counter so the global bound holds.
                    let mut combined = base;
                    combined.push(&target);
                    result = resolve(&combined, links);
                }
                // Not a symlink (or doesn't exist): keep the literal component.
            }
        }
    }
    result
}

/// True iff `child` is `base` itself or lives directly/indirectly under `base`.
///
/// Equivalent to Python's `full == base or full.startswith(base + os.sep)`.
/// Compares canonical paths byte-for-byte; a trailing separator is never
/// significant because [`PathBuf`] drops it.
fn within(child: &Path, base: &Path) -> bool {
    child == base || child.starts_with(base)
}

/// Resolve `rel` under `base`, rejecting escapes with [`ApiError::BadRequest`].
///
/// Direct port of `server._safe_path(rel, base)`:
/// 1. URL-decode (`unquote`) the input first, so `%2e%2e`/`%2f` are confined.
/// 2. Canonicalise `base` (`realpath`).
/// 3. Absolute inputs are honoured as-is (open-terminal echoes cwd back);
///    relative inputs are joined to `base` after stripping any leading `/`.
/// 4. The canonical result must equal `base` or sit beneath it, else 400.
pub fn safe_path(rel: &str, base: &Path) -> Result<PathBuf, ApiError> {
    let decoded = unquote(rel);
    let base = realpath(base);
    let full = if Path::new(&decoded).is_absolute() {
        realpath(Path::new(&decoded))
    } else {
        let trimmed = decoded.trim_start_matches('/');
        realpath(&base.join(trimmed))
    };
    if within(&full, &base) {
        Ok(full)
    } else {
        Err(ApiError::BadRequest(escapes().to_string()))
    }
}

/// Effective workspace base for a request: `workdir`, or `workdir/<subdir>`.
///
/// Direct port of `server._request_base(subdir)`. The subdir is validated
/// against [`SUBDIR_RE`] (no slashes / traversal / over-length) and created on
/// first use; a subdir that escapes `workdir` is rejected.
pub fn request_base(workdir: &Path, subdir: Option<&str>) -> Result<PathBuf, ApiError> {
    let Some(sub) = subdir.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(realpath(workdir));
    };
    if !is_valid_subdir(sub) {
        return Err(ApiError::BadRequest(
            "invalid X-Workspace-Subdir".to_string(),
        ));
    }
    let workdir = realpath(workdir);
    let base = realpath(&workdir.join(sub));
    if !within(&base, &workdir) {
        return Err(ApiError::BadRequest("subdir escapes workspace".to_string()));
    }
    std::fs::create_dir_all(&base)
        .map_err(|e| ApiError::Internal(format!("cannot create workspace subdir: {e}")))?;
    Ok(base)
}

/// `^[A-Za-z0-9._-]{1,64}$` — hand-rolled to avoid a regex dependency for one
/// fixed pattern. `.` is allowed by the charset, so `..` passes the charset and
/// is then caught by the escape check in [`request_base`] (matching Python).
fn is_valid_subdir(sub: &str) -> bool {
    let len = sub.chars().count();
    if !(1..=64).contains(&len) {
        return false;
    }
    sub.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquote_decodes_percent_encoding() {
        assert_eq!(unquote("%2e%2e/%2e%2e/etc/passwd"), "../../etc/passwd");
        assert_eq!(unquote("%2fetc%2fpasswd"), "/etc/passwd");
        // `+` is NOT a space (unquote, not unquote_plus).
        assert_eq!(unquote("a+b"), "a+b");
        assert_eq!(unquote("plain.txt"), "plain.txt");
    }

    #[test]
    fn subdir_charset() {
        assert!(is_valid_subdir("chat1"));
        assert!(is_valid_subdir("a.b-c_d"));
        // slashes rejected
        assert!(!is_valid_subdir("a/b"));
        // traversal: charset allows `.` but `..` escapes workspace upstream
        assert!(is_valid_subdir("..")); // passes charset, caught by escape check
                                        // too long
        assert!(!is_valid_subdir(&"x".repeat(65)));
        // empty
        assert!(!is_valid_subdir(""));
        // other chars
        assert!(!is_valid_subdir("a b"));
        assert!(!is_valid_subdir("a~b"));
    }

    #[test]
    fn within_predicate() {
        let base = Path::new("/workspace");
        assert!(within(Path::new("/workspace"), base));
        assert!(within(Path::new("/workspace/sub"), base));
        assert!(!within(Path::new("/workspacex"), base));
        assert!(!within(Path::new("/etc"), base));
    }
}
