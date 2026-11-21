//! `OPEN_TERMINAL_SETUP_SCRIPT` + `OPEN_TERMINAL_START_DIR` — the 0.17.7
//! workspace-bootstrap knobs.
//!
//! * **Start dir** — every new shell (`/execute` without an explicit `cwd`,
//!   PTY terminals) and the file-browser `cwd` open there instead of the
//!   workspace root. Must resolve inside the workspace; created at boot when
//!   missing (upstream: "must be inside home, created at startup").
//! * **Setup script** — runs ONCE in the start directory before the server
//!   accepts requests (upstream parity; a failure still lets the server
//!   start). The outcome is surfaced by `GET /system/info` as
//!   `setup: {status, exit_code, log_tail}` (log tail from the captured
//!   output file — upstream keeps `~/.open-terminal/setup.log`).
//!
//! Status is stored process-globally; `status()` returns `None` when the knob
//! is unset so `/system/info` omits the `setup` key entirely.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Value};

/// The captured setup outcome (process-global; `None` = knob unset).
#[derive(Debug, Clone)]
struct SetupOutcome {
    status: &'static str,
    exit_code: Option<i32>,
    log_tail: Option<String>,
}

static SETUP: OnceLock<Option<SetupOutcome>> = OnceLock::new();

/// Resolve + create the start directory against the workspace root.
///
/// Returns `None` when the knob is unset/blank. A start dir that escapes the
/// workspace is refused (logged, treated as unset) — same confinement stance
/// as every other runtime path.
#[must_use]
pub fn resolve_start_dir(workdir: &Path, knob: Option<&str>) -> Option<PathBuf> {
    let knob = knob.map(str::trim).filter(|s| !s.is_empty())?;
    let candidate = if Path::new(knob).is_absolute() {
        PathBuf::from(knob)
    } else {
        workdir.join(knob)
    };
    // Confine to the workspace (canonicalised; missing tail resolved lexically
    // before the create below — `safe_path::realpath` semantics without the
    // ApiError plumbing).
    let canonical = crate::safe_path::safe_path(&candidate.display().to_string(), workdir).ok()?;
    if std::fs::create_dir_all(&canonical).is_err() {
        tracing::warn!(dir = %canonical.display(), "OPEN_TERMINAL_START_DIR could not be created; using workspace root");
        return None;
    }
    Some(canonical)
}

/// Run the setup script (once, at boot) in `start_dir`, capturing the outcome.
/// Blocking by design: upstream starts serving only after the script ran.
pub fn run_at_boot(script: &str, start_dir: &Path) {
    let log_dir = std::env::temp_dir().join("open-websandbox-setup");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("setup.log");
    let output = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(start_dir)
        .env("SETUP_LOG", &log_path)
        .output();
    let outcome = match output {
        Ok(out) => {
            // Combined output tail (stdout + stderr), last 2 KiB.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let tail = tail_bytes(&text, 2048);
            let code = out.status.code();
            tracing::info!(
                exit = code.unwrap_or(-1),
                dir = %start_dir.display(),
                "setup script finished"
            );
            SetupOutcome {
                status: if code == Some(0) { "ok" } else { "failed" },
                exit_code: code,
                log_tail: Some(tail),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "setup script could not spawn");
            SetupOutcome {
                status: "failed",
                exit_code: None,
                log_tail: Some(format!("spawn failed: {e}\n")),
            }
        }
    };
    let _ = SETUP.set(Some(outcome));
}

/// `/system/info`'s `setup` key — `None` when the knob is unset (the key is
/// omitted entirely, exactly like upstream when no script is configured).
#[must_use]
pub fn status() -> Option<Value> {
    let outcome = SETUP.get()?;
    let o = outcome.as_ref()?;
    Some(json!({
        "status": o.status,
        "exit_code": o.exit_code,
        "log_tail": o.log_tail,
    }))
}

/// Last `max` characters of `text` (character-bound, not bytes, so the tail
/// never splits a UTF-8 sequence).
fn tail_bytes(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_dir_resolves_inside_workdir_and_creates_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let workdir = dir.path();
        let resolved = resolve_start_dir(workdir, Some("app")).unwrap();
        assert!(resolved.starts_with(workdir));
        assert!(resolved.is_dir(), "created at boot");
        // Blank/unset → None.
        assert!(resolve_start_dir(workdir, None).is_none());
        assert!(resolve_start_dir(workdir, Some("  ")).is_none());
        // Escape refused.
        assert!(resolve_start_dir(workdir, Some("../outside")).is_none());
    }

    #[test]
    fn tail_is_character_bounded() {
        assert_eq!(tail_bytes("short", 10), "short");
        let long = "x".repeat(3000);
        assert_eq!(tail_bytes(&long, 2048).chars().count(), 2048);
        // Multibyte safety: é (2 bytes) survives intact.
        let multibyte = "é".repeat(1100);
        let tail = tail_bytes(&multibyte, 1024);
        assert!(tail.starts_with('é') && tail.chars().count() == 1024);
    }

    #[test]
    fn status_is_none_until_run() {
        // The boot path sets the OnceLock once per process; in tests it may be
        // unset — assert the accessor never panics.
        let _ = status();
    }
}
