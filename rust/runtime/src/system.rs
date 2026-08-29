//! `GET /system` + `GET /info` — the open-terminal LLM-grounding surface
//! (upstream 0.11.27 / 0.11.6, template variables since 0.11.35).
//!
//! The default prompt text is **upstream-verbatim @ v0.12.3
//! (open_terminal/main.py `get_system_prompt`)** — copied byte-for-byte, not
//! authored here. Grounding values (`uname`, `$USER`, shell, `$HOME`, probed
//! python3) come from the live sandbox environment exactly like upstream's
//! `_system_prompt_variables`. When upstream changes its prompt, the pinned
//! unit test below forces the diff to be an explicit decision (update the
//! literal + provenance comment together).
//!
//! One documented divergence (docs/compatibility.md): upstream's sentence
//! `Python {python_version} is available.` is rendered only when a python3
//! probe succeeds. Upstream always has Python (its server *is* Python); our
//! default runtime image ships none (debian bookworm-slim + libreoffice-nogui
//! — no python3 anywhere in that dep tree), and a system prompt must not
//! claim an interpreter the model cannot run. With python3 present the
//! rendered prompt is byte-for-byte upstream.
//!
//! Workbench extension (openspec/changes/2026-08-29-workbench-toolchain,
//! documented divergence in docs/compatibility.md): when
//! `SANDBOX_TOOLS_MANIFEST` points at a readable file, TWO sections are
//! appended after template expansion — `## Available toolchain (base image)`
//! (the file's content) and `## Workspace conventions` (built in Rust from
//! the CONFIGURED workspace root, never a hardcoded path — the manifest file
//! is static at build time while WORKDIR varies per deployment). With the
//! knob unset/empty or the file missing, the prompt stays byte-for-byte
//! upstream (the pinned unit test below is the proof).

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::OnceLock;

use axum::extract::State;
use axum::Json;
use serde_json::json;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Tool-usage paragraph of the system prompt — upstream-verbatim @ v0.12.3
/// (open_terminal/main.py `get_system_prompt`, second f-string block; the
/// U+2014 em dash is significant).
const TOOL_TEXT: &str = "Use your tools to directly interact with the system \u{2014} run commands, read and write files, and search the filesystem. Prefer verifying the current state before making changes. When running commands, check the output to confirm success. If a command produces no output, that typically means it succeeded.";

/// The grounding facts substituted into the prompt (upstream
/// `_system_prompt_variables`, open_terminal/main.py:80-89).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grounding {
    /// `platform.system()` — e.g. `Linux`.
    pub os: String,
    /// `platform.release()` — kernel version.
    pub kernel: String,
    /// `platform.machine()` — e.g. `x86_64`.
    pub arch: String,
    /// `socket.gethostname()`.
    pub hostname: String,
    /// `$USER` (upstream defaults to `unknown`; single-user upstream also
    /// embeds it as ` as user '<name>'`).
    pub user: String,
    /// Shell binary (upstream: `$SHELL` else `/bin/sh`; ours: the effective
    /// runtime shell `/execute` actually runs).
    pub shell: String,
    /// Probed `python3 --version` (upstream: the server's own interpreter).
    /// `None` ⇒ the Python sentence is omitted (see module docs).
    pub python_version: Option<String>,
    /// `os.path.expanduser("~")` — `$HOME` else the passwd entry.
    pub home: String,
}

/// Resolve the grounding facts from the live environment.
///
/// `shell` comes from the caller (the runtime config) so the prompt states
/// the shell `/execute` actually runs, matching upstream's intent of
/// describing the environment the tools drive.
#[must_use]
pub fn grounding_from_env(shell: &str) -> Grounding {
    let (os, kernel, arch, hostname) = uname_facts();
    Grounding {
        os,
        kernel,
        arch,
        hostname,
        user: std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        shell: shell.to_string(),
        python_version: None, // filled by the async probe below
        home: std::env::var("HOME").unwrap_or_else(|_| passwd_home()),
    }
}

/// `uname()` facts; on the (practically unreachable) failure the values
/// degrade to empty strings rather than failing the request.
fn uname_facts() -> (String, String, String, String) {
    match nix::sys::utsname::uname() {
        Ok(u) => (
            u.sysname().to_string_lossy().into_owned(),
            u.release().to_string_lossy().into_owned(),
            u.machine().to_string_lossy().into_owned(),
            u.nodename().to_string_lossy().into_owned(),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "uname failed; grounding with empty values");
            (String::new(), String::new(), String::new(), String::new())
        }
    }
}

/// `$HOME` fallback: the passwd entry of the current uid (upstream
/// `os.path.expanduser("~")` semantics).
fn passwd_home() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .ok()
        .flatten()
        .map_or_else(|| "/".to_string(), |u| u.dir.to_string_lossy().into_owned())
}

/// Probe `python3 --version` then `python --version` (cached for the process
/// lifetime; `sys.version.split()[0]` equivalent = the bare `X.Y.Z`).
///
/// Returns `None` when no Python is on `PATH` — our default image ships none.
async fn probe_python_version() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            for bin in ["python3", "python"] {
                if let Ok(out) = std::process::Command::new(bin).arg("--version").output() {
                    if !out.status.success() {
                        continue;
                    }
                    // python ≥3.4 prints to stdout; older prints to stderr.
                    let line = String::from_utf8_lossy(&out.stdout);
                    let line = if line.trim().is_empty() {
                        String::from_utf8_lossy(&out.stderr).into_owned()
                    } else {
                        line.into_owned()
                    };
                    if let Some(version) = line
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().last())
                    {
                        return Some(version.to_string());
                    }
                }
            }
            None
        })
        .clone()
}

/// Render the **upstream-verbatim** default prompt for the given grounding.
///
/// Byte layout (with a probed Python) is exactly upstream
/// `get_system_prompt()`: grounding sentence(s) `\n\n` tool-usage paragraph,
/// plus `\n\n{info}` when operator info is set. Without a Python the
/// `Python … is available.` sentence is dropped (documented divergence).
#[must_use]
pub fn render_default_prompt(g: &Grounding, info: &str) -> String {
    let mut first = format!(
        "You have access to a computer running {} {} ({}) on host \"{}\" as user '{}' with {}.",
        g.os, g.kernel, g.arch, g.hostname, g.user, g.shell
    );
    if let Some(py) = &g.python_version {
        first.push_str(&format!(" Python {py} is available."));
    }
    let mut prompt = format!("{first}\n\n{TOOL_TEXT}");
    if !info.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(info);
    }
    prompt
}

/// Expand `{{ var }}` template placeholders the way upstream
/// `_expand_system_prompt_template` does (open_terminal/main.py:87-104):
/// the upstream key set substitutes; unknown keys pass through verbatim.
#[must_use]
pub fn expand_template(template: &str, g: &Grounding) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\{\{\s*([a-zA-Z0-9_]+)\s*\}\}").expect("static template regex")
    });
    re.replace_all(template, |c: &regex::Captures<'_>| -> String {
        match c.get(1).and_then(|m| {
            let key = m.as_str().trim();
            // Skip empty-name captures (impossible with this regex, but be
            // explicit): fall through to the verbatim arm.
            (!key.is_empty()).then_some(key)
        }) {
            Some("os") => g.os.clone(),
            Some("kernel") => g.kernel.clone(),
            Some("arch") => g.arch.clone(),
            Some("hostname") => g.hostname.clone(),
            Some("user") => g.user.clone(),
            Some("shell") => g.shell.clone(),
            Some("python_version") => g
                .python_version
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            Some("home") => g.home.clone(),
            _ => c.get(0).map(|m| m.as_str()).unwrap_or_default().to_string(),
        }
    })
    .into_owned()
}

/// Build the full system prompt: the operator template (expanded) when
/// `OPEN_TERMINAL_SYSTEM_PROMPT` is set, else the upstream-verbatim default.
///
/// When `tools_manifest` is set (workbench knob `SANDBOX_TOOLS_MANIFEST`),
/// the toolchain + workspace-conventions sections are appended AFTER the
/// (default or operator-overridden) prompt — see [`append_sections`].
#[must_use]
pub async fn system_prompt(
    shell: &str,
    template: &str,
    info: &str,
    workdir: &Path,
    tools_manifest: Option<&Path>,
) -> String {
    let mut g = grounding_from_env(shell);
    g.python_version = probe_python_version().await;
    let mut prompt = if template.is_empty() {
        render_default_prompt(&g, info)
    } else {
        expand_template(template, &g)
    };
    append_sections(&mut prompt, workdir, tools_manifest);
    prompt
}

/// Append the workbench sections to an already-rendered prompt.
///
/// Gated on the single `SANDBOX_TOOLS_MANIFEST` knob: `None` (or an
/// unreadable/empty file — warned, never fatal) leaves the prompt exactly as
/// rendered, so the upstream-verbatim pin holds when the feature is off.
/// The conventions section is built from the CONFIGURED `workdir` (the same
/// `RuntimeConfig::workdir` every file op resolves against) — no hardcoded
/// workspace path exists here, and a non-default `WORKDIR` must change the
/// rendered paths (pinned by unit test).
fn append_sections(prompt: &mut String, workdir: &Path, tools_manifest: Option<&Path>) {
    let Some(path) = tools_manifest else { return };
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            prompt.push_str("\n\n## Available toolchain (base image)\n\n");
            prompt.push_str(text.trim_end());
            prompt.push_str("\n\n");
            prompt.push_str(&workspace_conventions(workdir));
        }
        Ok(_) => {
            tracing::warn!(path = %path.display(), "tools manifest empty; /system prompt stays as rendered");
        }
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "cannot read tools manifest; /system prompt stays as rendered"
        ),
    }
}

/// The config-driven `## Workspace conventions` section. Paths come from
/// `workdir` via `Path::join` (root `/` stays correct); `/packages` recipes
/// are mount locations, not workspace paths, so they are literal.
#[must_use]
pub(crate) fn workspace_conventions(workdir: &Path) -> String {
    let tmp = workdir.join("tmp");
    let venv = workdir.join(".venv");
    let workdir_disp = workdir.display();
    let tmp_disp = tmp.display();
    let venv_disp = venv.display();
    format!(
        "## Workspace conventions\n\n\
         - Scratch/intermediate files belong in {tmp_disp} — create it if missing (`mkdir -p {tmp_disp}`); keep the workspace root for deliverables.\n\
         - `/tmp` is tmpfs and wiped on pod restart; the workspace root (`{workdir_disp}`) persists across sessions.\n\
         - Persistent Python env: `python3 -m venv {venv_disp}` (survives pod restarts).\n\
         - Session-local Python deps: `pip install --target /packages/py <pkg>`, then `PYTHONPATH=/packages/py` for the session.\n\
         - npm user prefix: `npm config set prefix /packages/npm`.\n\
         - `sudo apt-get install <pkg>` writes the ephemeral rootfs — reinstall after a pod restart."
    )
}

/// `GET /system` — `{"prompt": <text>}` (upstream 0.11.27; auth-protected).
pub async fn get_system(_auth: Authed, State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = &state.config;
    let prompt = system_prompt(
        &cfg.shell,
        &cfg.system_prompt,
        &cfg.info,
        &cfg.workdir,
        cfg.tools_manifest.as_deref(),
    )
    .await;
    Json(json!({ "prompt": prompt }))
}

/// `GET /info` — `{"info": <text>}` (upstream 0.11.6).
///
/// Upstream registers the route only `if OPEN_TERMINAL_INFO:` — with the
/// value unset the path 404s (`{"detail": "Not Found"}`). Mirrored exactly.
///
/// # Errors
///
/// Returns [`ApiError::NotFound`] when no operator info is configured.
#[utoipa::path(
    get,
    path = "/info",
    tag = "meta",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Operator-provided environment info", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "No operator info configured (route unregistered upstream)", body = shared::ErrorResponse)
    )
)]
pub async fn get_info(
    _auth: Authed,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.config.info.is_empty() {
        return Err(ApiError::NotFound("Not Found".to_string()));
    }
    Ok(Json(json!({ "info": state.config.info })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic() -> Grounding {
        Grounding {
            os: "Linux".into(),
            kernel: "6.1.0-13-amd64".into(),
            arch: "x86_64".into(),
            hostname: "sandbox-7f9a".into(),
            user: "sandbox".into(),
            shell: "/bin/bash".into(),
            python_version: Some("3.11.2".into()),
            home: "/home/sandbox".into(),
        }
    }

    /// THE verbatim pin: this literal is upstream's text byte-for-byte
    /// (open-terminal v0.12.3, open_terminal/main.py `get_system_prompt`).
    /// If this fails, the prompt drifted from upstream — update BOTH the
    /// renderer and this literal as one conscious diff decision.
    #[test]
    fn default_prompt_is_upstream_verbatim() {
        let expected = concat!(
            "You have access to a computer running Linux 6.1.0-13-amd64 (x86_64) ",
            "on host \"sandbox-7f9a\" as user 'sandbox' with /bin/bash. ",
            "Python 3.11.2 is available.\n\n",
            "Use your tools to directly interact with the system \u{2014} run commands, ",
            "read and write files, and search the filesystem. ",
            "Prefer verifying the current state before making changes. ",
            "When running commands, check the output to confirm success. ",
            "If a command produces no output, that typically means it succeeded."
        );
        assert_eq!(render_default_prompt(&synthetic(), ""), expected);
    }

    #[test]
    fn python_sentence_omitted_when_unavailable() {
        let mut g = synthetic();
        g.python_version = None;
        let p = render_default_prompt(&g, "");
        assert!(!p.contains("Python"));
        assert!(
            p.contains("with /bin/bash.\n\nUse your tools"),
            "no stray space before the paragraph break: {p:?}"
        );
    }

    #[test]
    fn operator_info_is_appended() {
        let p = render_default_prompt(&synthetic(), "Managed by the physics dept.");
        assert!(p.ends_with("\n\nManaged by the physics dept."));
    }

    #[test]
    fn template_expands_upstream_key_set() {
        let g = synthetic();
        let out = expand_template("{{os}} {{kernel}} {{arch}} {{hostname}} {{user}} {{shell}} {{python_version}} {{home}}", &g);
        assert_eq!(
            out,
            "Linux 6.1.0-13-amd64 x86_64 sandbox-7f9a sandbox /bin/bash 3.11.2 /home/sandbox"
        );
    }

    #[test]
    fn template_tolerates_spacing_and_unknown_keys() {
        let g = synthetic();
        assert_eq!(
            expand_template("{{ os }}|{{os}}|{{  os  }}", &g),
            "Linux|Linux|Linux"
        );
        assert_eq!(expand_template("{{nope}} {{ os }}", &g), "{{nope}} Linux");
    }

    #[test]
    fn template_python_falls_back_to_unknown() {
        let mut g = synthetic();
        g.python_version = None;
        assert_eq!(expand_template("{{python_version}}", &g), "unknown");
    }

    // --- workbench toolchain append (SANDBOX_TOOLS_MANIFEST knob) -------------

    /// Minimal stand-in manifest file (no tempfile dep in the runtime crate).
    fn write_manifest(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("owsb-system-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir temp dir");
        let path = dir.join("sandbox-capabilities.md");
        std::fs::write(&path, content).expect("write manifest");
        path
    }

    #[test]
    fn manifest_appends_toolchain_and_conventions_sections() {
        let manifest =
            write_manifest("- Python and data — python3 3.11.2, pip: pandas 2.2.3, openpyxl 3.1.5");
        let mut prompt = render_default_prompt(&synthetic(), "");
        append_sections(
            &mut prompt,
            std::path::Path::new("/workspace"),
            Some(&manifest),
        );

        let toolchain = prompt
            .find("## Available toolchain (base image)")
            .expect("toolchain section");
        let conventions = prompt
            .find("## Workspace conventions")
            .expect("conventions section");
        assert!(
            toolchain < conventions,
            "conventions come after the toolchain section"
        );
        assert!(prompt.contains("pandas 2.2.3"));
        // The upstream-verbatim body is still the prefix, untouched.
        assert!(prompt.starts_with("You have access to a computer running Linux"));
    }

    #[test]
    fn knob_off_or_missing_file_keeps_prompt_byte_for_byte() {
        let base = render_default_prompt(&synthetic(), "");

        // Knob off (None): no sections at all.
        let mut off = base.clone();
        append_sections(&mut off, std::path::Path::new("/workspace"), None);
        assert_eq!(off, base, "disabled knob must not change the prompt");

        // Knob on but the file is missing: warn + skip, still unchanged.
        let mut missing = base.clone();
        append_sections(
            &mut missing,
            std::path::Path::new("/workspace"),
            Some(std::path::Path::new("/nonexistent/sandbox-capabilities.md")),
        );
        assert_eq!(missing, base, "missing manifest must not change the prompt");
    }

    #[test]
    fn override_prompt_also_gets_the_sections() {
        let g = synthetic();
        let mut prompt = expand_template("Custom host={{hostname}} template.", &g);
        let manifest = write_manifest("- Archives — gzip 1.12");
        append_sections(
            &mut prompt,
            std::path::Path::new("/workspace"),
            Some(&manifest),
        );
        assert!(prompt.starts_with("Custom host=sandbox-7f9a template."));
        assert!(prompt.contains("## Available toolchain (base image)"));
        assert!(prompt.contains("## Workspace conventions"));
    }

    #[test]
    fn conventions_are_built_from_the_configured_workdir() {
        let c = workspace_conventions(std::path::Path::new("/data/ws"));
        assert!(
            c.contains("/data/ws/tmp"),
            "scratch dir must follow WORKDIR: {c}"
        );
        assert!(
            c.contains("/data/ws/.venv"),
            "venv path must follow WORKDIR: {c}"
        );
        assert!(
            !c.contains("/workspace"),
            "no hardcoded default workspace root may appear: {c}"
        );

        // Default config renders the default root; root `/` stays sane.
        let d = workspace_conventions(std::path::Path::new("/workspace"));
        assert!(d.contains("/workspace/tmp"));
        let root = workspace_conventions(std::path::Path::new("/"));
        assert!(root.contains("/tmp —"));
    }

    #[test]
    fn conventions_listed_only_via_the_knob() {
        let base = render_default_prompt(&synthetic(), "");
        let mut off = base.clone();
        append_sections(&mut off, std::path::Path::new("/workspace"), None);
        assert!(!off.contains("## Workspace conventions"));
        assert!(!off.contains("## Available toolchain"));
    }
}
