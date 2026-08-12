//! Path confinement — the security-critical boundary of the runtime.
//!
//! Every file endpoint funnels through [`safe_path`], which confines a
//! caller-supplied path to the workspace base (`WORKDIR`, or
//! `WORKDIR/<subdir>` under `X-Workspace-Subdir`). It rejects any path that
//! resolves outside `base` — `..` traversal, absolute escapes, URL-encoded
//! escapes, root
//! traversal, and symlink escapes — while honouring absolute paths that are
//! already inside `base` (the open-terminal UI echoes the cwd back from
//! `GET /files/cwd`).
//!
//! The 17 path-confinement cases are ported verbatim
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
/// Like `os.path.realpath`, which — unlike
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
/// Equivalent to `full == base or full.startswith(base + os.sep)`.
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
/// is then caught by the escape check in [`request_base`].
fn is_valid_subdir(sub: &str) -> bool {
    let len = sub.chars().count();
    if !(1..=64).contains(&len) {
        return false;
    }
    sub.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// --- TOCTOU-safe file open (#99 A5) -----------------------------------------
//
// `safe_path` resolves symlinks (realpath, up to SYMLINK_LIMIT) and checks the
// result sits inside `base`. But the caller then opens the returned path *again*,
// and a symlink swapped between the check and the open can escape `/workspace`.
// We close that window by opening with O_NOFOLLOW (the final component may not be
// a symlink, so a swap-to-symlink is rejected rather than followed) and then
// re-resolving the *opened* fd via /proc/self/fd to re-confirm it still names a
// path inside `base` (defends against an intermediate directory being swapped).
//
// /proc/self/fd is available on Linux hosts and under gVisor `runsc` (which
// emulates /proc). The constants below are the stable Linux UAPI values
// (asm-generic/fcntl.h); the runtime is Linux-only (gVisor/runc).
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

/// `O_NOFOLLOW` — refuse to follow a symlink in the final path component (Linux UAPI).
const O_NOFOLLOW: i32 = 0o400_000;
/// `O_CLOEXEC` — close the fd on exec(2) so it never leaks into a child (Linux UAPI).
const O_CLOEXEC: i32 = 0o2_000_000;
/// `ELOOP` — the errno open(2) returns under `O_NOFOLLOW` when the final component is a symlink.
const ELOOP: i32 = 40;

/// Real path an already-opened fd names, read from `/proc/self/fd/<fd>`.
///
/// `/proc/self/fd/N` is a kernel-maintained symlink to whatever the fd actually
/// references, so this is authoritative (it sees through any swap that happened
// between [`safe_path`] and the open). Returns `None` only on systems without
// `/proc` (not a supported deployment). A path whose file was unlinked after the
// open is reported by the kernel with `" (deleted)"` appended, which we strip so
// the `within` check still passes for a confined file that was concurrently removed.
fn fd_realpath(fd: i32) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    let s = link.to_string_lossy();
    let stripped = s.strip_suffix(" (deleted)").unwrap_or(&s);
    Some(PathBuf::from(stripped))
}

/// Open `resolved` confined to `base`, closing the TOCTOU window between
/// [`safe_path`]'s check and the caller's open.
///
/// `opts` carries the caller's intent (read/write/create/truncate); this helper
// injects `O_NOFOLLOW | O_CLOEXEC`. After opening it re-resolves the fd via
// [`fd_realpath`] and re-checks confinement, so a swap between `safe_path` and
// this open that pointed the fd outside `base` is rejected with
// [`ApiError::BadRequest`].
fn open_confined(resolved: &Path, base: &Path, mut opts: OpenOptions) -> Result<File, ApiError> {
    opts.custom_flags(O_NOFOLLOW | O_CLOEXEC);
    let file = opts.open(resolved).map_err(|e| {
        // ELOOP under O_NOFOLLOW == the final component is a symlink (e.g. the
        // file was swapped for a symlink between safe_path and this open).
        if e.raw_os_error() == Some(ELOOP) {
            ApiError::BadRequest("path resolves to a symlink (refused)".to_string())
        } else {
            ApiError::Internal(format!("open failed: {e}"))
        }
    })?;
    // Re-resolve what the fd *actually* points at and re-confirm confinement —
    // defends against an intermediate directory being swapped to escape base.
    let real = fd_realpath(file.as_raw_fd())
        .ok_or_else(|| ApiError::Internal("cannot re-resolve opened fd".to_string()))?;
    if !within(&real, &realpath(base)) {
        // The File is dropped (closed) on return; best-effort.
        return Err(ApiError::BadRequest("path escaped workspace".to_string()));
    }
    Ok(file)
}

/// Open `resolved` read-only, confined to `base` (TOCTOU-safe).
///
/// Use after [`safe_path`] for any read of file contents, so a symlink swapped
/// between the `safe_path` check and this open cannot escape `/workspace`.
pub fn open_read(resolved: &Path, base: &Path) -> Result<File, ApiError> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    open_confined(resolved, base, opts)
}

/// Open `resolved` write-only, confined to `base` (TOCTOU-safe).
///
/// `create` creates the file if absent (mode 0o600); `truncate` truncates an
/// existing file. Use after [`safe_path`] for any write of file contents.
pub fn open_write(
    resolved: &Path,
    base: &Path,
    create: bool,
    truncate: bool,
) -> Result<File, ApiError> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    opts.create(create);
    opts.truncate(truncate);
    opts.mode(0o600);
    open_confined(resolved, base, opts)
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

    // --- #99 A5: TOCTOU-safe open_confined --------------------------------
    use crate::safe_path::{fd_realpath, open_read};
    use std::os::unix::io::AsRawFd;

    /// Build a throwaway base dir + return its path, cleaned up on drop.
    struct TmpBase(PathBuf);
    impl TmpBase {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "owui-safe-path-{}-{}",
                std::process::id(),
                // unique per-test without pulling in a uuid dep.
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpBase {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fd_realpath_round_trips_an_opened_file() {
        let t = TmpBase::new();
        let f = t.path().join("real.txt");
        std::fs::write(&f, b"x").unwrap();
        let file = std::fs::File::open(&f).unwrap();
        let real = fd_realpath(file.as_raw_fd()).expect("/proc must be present");
        // fd_realpath returns the canonical kernel path; it must still be the file we opened.
        assert_eq!(std::fs::canonicalize(&f).unwrap(), real);
    }

    #[test]
    fn open_confined_reads_a_normal_file() {
        let t = TmpBase::new();
        std::fs::write(t.path().join("file.txt"), b"safe-content").unwrap();
        let full = safe_path("file.txt", t.path()).unwrap();
        let file = open_read(&full, t.path()).unwrap();
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::BufReader::new(file), &mut buf).unwrap();
        assert_eq!(buf, "safe-content");
    }

    #[test]
    fn open_confined_rejects_final_component_symlink_swap() {
        // safe_path sees a real confined file and passes the within check; an
        // attacker then swaps it for a symlink before the open. O_NOFOLLOW makes
        // the open fail with ELOOP rather than following the swapped symlink.
        let t = TmpBase::new();
        std::fs::write(t.path().join("file.txt"), b"safe").unwrap();
        let full = safe_path("file.txt", t.path()).unwrap(); // check passes (real file, inside)
                                                             // TOCTOU window: swap the file for a symlink to /etc/passwd.
        std::fs::remove_file(t.path().join("file.txt")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", t.path().join("file.txt")).unwrap();
        let err = open_read(&full, t.path()).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(_)),
            "swapped symlink must be refused, got {err:?}"
        );
    }

    #[test]
    fn open_confined_rejects_intermediate_directory_swap() {
        // safe_path resolves base/inner/file.txt (all real, inside base). An
        // attacker then swaps `inner` for a symlink to an outside dir that also
        // contains `file.txt`. O_NOFOLLOW only guards the final component, so the
        // open itself succeeds — the /proc/self/fd re-resolve catches the escape.
        let t = TmpBase::new();
        let outside = std::env::temp_dir().join(format!(
            "owui-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(outside.join("inner")).unwrap();
        std::fs::write(outside.join("inner").join("file.txt"), b"exfiltrated").unwrap();
        std::fs::create_dir_all(t.path().join("inner")).unwrap();
        std::fs::write(t.path().join("inner").join("file.txt"), b"safe").unwrap();
        let full = safe_path("inner/file.txt", t.path()).unwrap(); // inside, all real
                                                                   // TOCTOU window: swap the intermediate dir for a symlink to the outside tree.
        std::fs::remove_dir_all(t.path().join("inner")).unwrap();
        std::os::unix::fs::symlink(outside.join("inner"), t.path().join("inner")).unwrap();
        let err = open_read(&full, t.path()).unwrap_err();
        assert!(
            matches!(err, ApiError::BadRequest(_)),
            "intermediate-dir escape must be refused, got {err:?}"
        );
        std::fs::remove_dir_all(&outside).unwrap();
    }
}
