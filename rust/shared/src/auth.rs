//! Per-session-key authentication helpers.
//!
//! The broker and runtime mutually authenticate every request with a
//! per-session key projected from a Secret (see issues #4 / #50). Comparing a
//! supplied key against the expected key MUST be constant-time to avoid timing
//! oracles; this module wraps [`subtle::ConstantTimeEq`] for that single
//! purpose.

#![forbid(unsafe_code)]

use subtle::ConstantTimeEq;

/// Compare two byte slices in constant time.
///
/// Returns `true` iff `a` and `b` are bytewise equal. Unlike slice equality
/// (`==`), the comparison does not short-circuit on the first differing byte,
/// so it reveals no information about the key beyond whether it matched.
/// Mismatched lengths also compare in constant time and return `false`.
///
/// Used by the per-session-key auth checks in the broker (#43) and the runtime
/// (#50).
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_slices_match() {
        assert!(constant_time_eq(b"super-secret-key", b"super-secret-key"));
    }

    #[test]
    fn different_length_is_false() {
        assert!(!constant_time_eq(b"short", b"longer-key"));
        assert!(!constant_time_eq(b"longer-key", b"short"));
    }

    #[test]
    fn same_length_different_bytes_is_false() {
        assert!(!constant_time_eq(b"super-secret-key", b"super-secret-kez"));
        assert!(!constant_time_eq(b"super-secret-key", b"xuper-secret-key"));
    }

    #[test]
    fn empty_slices_match() {
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(!constant_time_eq(b"x", b""));
    }
}
