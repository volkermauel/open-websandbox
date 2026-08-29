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

#![forbid(unsafe_code)]

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
#[must_use]
pub async fn system_prompt(shell: &str, template: &str, info: &str) -> String {
    let mut g = grounding_from_env(shell);
    g.python_version = probe_python_version().await;
    if !template.is_empty() {
        return expand_template(template, &g);
    }
    render_default_prompt(&g, info)
}

/// `GET /system` — `{"prompt": <text>}` (upstream 0.11.27; auth-protected).
pub async fn get_system(_auth: Authed, State(state): State<AppState>) -> Json<serde_json::Value> {
    let cfg = &state.config;
    let prompt = system_prompt(&cfg.shell, &cfg.system_prompt, &cfg.info).await;
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
}
