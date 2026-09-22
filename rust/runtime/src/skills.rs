//! `GET /skills*` — terminal-owned Agent Skills (open-terminal `main` post-v0.12.3,
//! driven by OWUI v0.11.4; issue #195).
//!
//! Scans the four global skill roots under the workspace home
//! (`.agents/skills`, `.cptr/skills`, `.claude/skills`, `.codex/skills`) for
//! subdirectories carrying a `SKILL.md` with frontmatter `name`/`description`.
//! Discovery (`GET /skills`) and read (`GET /skills/read?name=`, plus the
//! `GET /skills/{name}` path-param alias OWUI v0.11.4's backend calls) mirror
//! upstream semantics: first-name-wins dedup, description capped at 1024 chars,
//! entries without a frontmatter description are skipped.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::extract::{Path as PathParam, Query, State};
use axum::http::HeaderMap;
use axum::Json;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Upstream `GLOBAL_SKILL_DIRS`: scanned (in this order) under the workspace home.
pub const GLOBAL_SKILL_DIRS: [&str; 4] = [
    ".agents/skills",
    ".cptr/skills",
    ".claude/skills",
    ".codex/skills",
];

/// Upstream cap: a skill description is truncated to 1024 characters.
const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Upstream `MAX_SKILL_RESOURCE_DEPTH`: the resource walk never descends past 4 levels.
const MAX_RESOURCE_DEPTH: usize = 4;
/// Upstream `MAX_SKILL_RESOURCE_FILES`: at most 50 resource files are reported.
const MAX_RESOURCE_FILES: usize = 50;

/// `GET /skills` row — upstream `SkillSummary`.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct SkillSummary {
    /// Stable client id: `terminal:<percent-encoded name>`.
    pub id: String,
    /// Frontmatter `name` (falls back to the directory name).
    pub name: String,
    /// Frontmatter `description` (≤ 1024 chars).
    pub description: String,
    /// Absolute path of the `SKILL.md`.
    pub location: String,
    /// Always `global` (we have no per-chat skill roots).
    pub scope: String,
    /// Always `terminal`.
    pub source: String,
}

/// `GET /skills/read` / `GET /skills/{name}` — upstream `SkillReadResponse`.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct SkillReadResponse {
    /// Stable client id: `terminal:<percent-encoded name>`.
    pub id: String,
    /// Frontmatter `name`.
    pub name: String,
    /// Frontmatter `description` (≤ 1024 chars).
    pub description: String,
    /// Absolute path of the `SKILL.md`.
    pub location: String,
    /// Always `global`.
    pub scope: String,
    /// Always `terminal`.
    pub source: String,
    /// `SKILL.md` body with the frontmatter stripped.
    pub content: String,
    /// Relative resource file paths under the skill directory.
    pub resources: Vec<String>,
}

/// Query parameters for `GET /skills/read`.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ReadSkillQuery {
    /// Skill name as returned by `GET /skills`.
    pub name: String,
}

/// Percent-encode a skill name like Python's `urllib.parse.quote(name, safe="")`
/// (unreserved `[A-Za-z0-9_.~-]` survive; everything else is `%XX`, uppercase hex).
fn quote_name(name: &str) -> String {
    const SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'_')
        .remove(b'.')
        .remove(b'-')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(name, SAFE).to_string()
}

/// Parse `SKILL.md` frontmatter the way upstream `parse_skill_frontmatter` does:
/// a leading `---\n`, closed by the next `\n---`; `key: value` lines (blank lines
/// and `#` comments skipped); a value of `>` or `|` collects the following indented
/// block (folded with spaces / joined with newlines respectively); single/double
/// quotes are stripped from plain values. Returns `(frontmatter, body-after-frontmatter)`.
fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let empty = (BTreeMap::new(), text.trim().to_string());
    if !text.starts_with("---\n") {
        return empty;
    }
    let Some(end) = text.find("\n---").filter(|&e| e >= 4) else {
        return empty;
    };
    let mut frontmatter: BTreeMap<String, String> = BTreeMap::new();
    let lines: Vec<&str> = text[4..end].lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let Some((key, value)) = line.split_once(':') else {
            idx += 1;
            continue;
        };
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            idx += 1;
            continue;
        }
        let key = key.trim().to_string();
        let value = value.trim();
        if value == ">" || value == "|" {
            let mut block: Vec<String> = Vec::new();
            idx += 1;
            while idx < lines.len() && (lines[idx].starts_with(' ') || lines[idx].starts_with('\t'))
            {
                block.push(lines[idx].trim().to_string());
                idx += 1;
            }
            frontmatter.insert(
                key,
                if value == "|" {
                    block.join("\n")
                } else {
                    block.join(" ")
                },
            );
            continue;
        }
        frontmatter.insert(
            key,
            value.trim_matches(|c| c == '"' || c == '\'').to_string(),
        );
        idx += 1;
    }
    // Body starts after the closing `---` line (upstream: `text.find("\n", end + 1)`).
    let body_start = text[end + 1..].find('\n').map(|off| end + 1 + off + 1);
    let body = body_start.map_or_else(String::new, |s| text[s..].trim().to_string());
    (frontmatter, body)
}

/// Scan one skill root directory for skills (upstream `_list_skills_in_dir`).
fn list_skills_in_dir(root: &Path, scope: &str) -> Vec<SkillSummary> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut skills = Vec::new();
    for dirname in names {
        if dirname.starts_with('.') {
            continue;
        }
        let skill_dir = root.join(&dirname);
        if !skill_dir.is_dir() {
            continue;
        }
        let skill_path = skill_dir.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&skill_path) else {
            continue;
        };
        let (frontmatter, _) = parse_frontmatter(&text);
        let description = frontmatter
            .get("description")
            .map_or("", String::as_str)
            .trim()
            .to_string();
        if description.is_empty() {
            continue;
        }
        let name = frontmatter
            .get("name")
            .map_or(dirname.as_str(), String::as_str)
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        skills.push(SkillSummary {
            id: format!("terminal:{}", quote_name(&name)),
            description: description.chars().take(MAX_DESCRIPTION_CHARS).collect(),
            location: skill_path.to_string_lossy().to_string(),
            name,
            scope: scope.to_string(),
            source: "terminal".to_string(),
        });
    }
    skills
}

/// Scan every global skill root under `home` (upstream `list_skills`):
/// roots are normalised + deduped, skills deduped by first-seen name.
fn scan_skills(home: &Path) -> Vec<SkillSummary> {
    let mut seen_roots: Vec<PathBuf> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();
    let mut skills = Vec::new();
    for rel in GLOBAL_SKILL_DIRS {
        let root = home.join(rel);
        if seen_roots.contains(&root) {
            continue;
        }
        seen_roots.push(root.clone());
        for skill in list_skills_in_dir(&root, "global") {
            if seen_names.contains(&skill.name) {
                continue;
            }
            seen_names.push(skill.name.clone());
            skills.push(skill);
        }
    }
    skills
}

/// Walk a skill directory collecting relative resource paths (upstream
/// `list_skill_resources`): depth ≤ 4, ≤ 50 files, dotfiles and `SKILL.md` skipped.
fn list_skill_resources(skill_dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
        if depth >= MAX_RESOURCE_DEPTH || out.len() >= MAX_RESOURCE_FILES {
            return;
        }
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<String> = read
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            if out.len() >= MAX_RESOURCE_FILES {
                return;
            }
            if name.starts_with('.') || name == "SKILL.md" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let path = dir.join(&name);
            if path.is_file() {
                out.push(rel);
            } else if path.is_dir() {
                walk(&path, &rel, depth + 1, out);
            }
        }
    }
    let mut resources = Vec::new();
    walk(skill_dir, "", 0, &mut resources);
    resources
}

/// `GET /skills` — list terminal-owned skills.
///
/// # Errors
///
/// Never fails on unreadable roots (they simply contribute no skills).
#[utoipa::path(
    get,
    path = "/skills",
    tag = "skills",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Skills visible in this workspace", body = [SkillSummary]),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn list_skills(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SkillSummary>>, ApiError> {
    let home = crate::files::cwd_base(&state, &headers)?;
    Ok(Json(scan_skills(&home)))
}

/// Resolve one skill by exact name and build the read response.
fn read_skill_inner(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
) -> Result<SkillReadResponse, ApiError> {
    let home = crate::files::cwd_base(state, headers)?;
    for skill in scan_skills(&home) {
        if skill.name != name {
            continue;
        }
        let text = std::fs::read_to_string(&skill.location)
            .map_err(|e| ApiError::Internal(format!("read skill failed: {e}")))?;
        let (_, body) = parse_frontmatter(&text);
        let skill_dir = Path::new(&skill.location)
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let resources = list_skill_resources(&skill_dir);
        return Ok(SkillReadResponse {
            content: body,
            resources,
            id: skill.id,
            description: skill.description,
            location: skill.location,
            name: skill.name,
            scope: skill.scope,
            source: skill.source,
        });
    }
    Err(ApiError::NotFound("Skill not found".to_string()))
}

/// `GET /skills/read?name=` — upstream's read shape (query parameter).
///
/// # Errors
///
/// Returns [`ApiError::NotFound`] when no skill carries that exact name.
#[utoipa::path(
    get,
    path = "/skills/read",
    tag = "skills",
    params(ReadSkillQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Skill body + resources", body = SkillReadResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Skill not found", body = shared::ErrorResponse)
    )
)]
pub async fn read_skill(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ReadSkillQuery>,
) -> Result<Json<SkillReadResponse>, ApiError> {
    Ok(Json(read_skill_inner(&state, &headers, &q.name)?))
}

/// `GET /skills/{name}` — the path-parameter alias OWUI v0.11.4's backend calls
/// (`get_terminal_skill`); semantically identical to [`read_skill`].
///
/// # Errors
///
/// Returns [`ApiError::NotFound`] when no skill carries that exact name.
#[utoipa::path(
    get,
    path = "/skills/{name}",
    tag = "skills",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Skill body + resources", body = SkillReadResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Skill not found", body = shared::ErrorResponse)
    )
)]
pub async fn read_skill_by_name(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(name): PathParam<String>,
) -> Result<Json<SkillReadResponse>, ApiError> {
    Ok(Json(read_skill_inner(&state, &headers, &name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_plain_and_block_values() {
        let text = "---\nname: my-skill\ndescription: >\n  folded\n  lines\n# comment\n---\n\nBody text.\n";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.get("name").map(String::as_str), Some("my-skill"));
        assert_eq!(
            fm.get("description").map(String::as_str),
            Some("folded lines")
        );
        assert_eq!(body, "Body text.");
    }

    #[test]
    fn frontmatter_absent_yields_nothing() {
        let (fm, body) = parse_frontmatter("just a body");
        assert!(fm.is_empty());
        assert_eq!(body, "just a body");
    }

    #[test]
    fn quote_matches_python_url_quote_safe_empty() {
        assert_eq!(quote_name("my skill_v1.0-x~"), "my%20skill_v1.0-x~");
        assert_eq!(quote_name("ä"), "%C3%A4");
    }
}
