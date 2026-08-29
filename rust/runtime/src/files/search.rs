//! `files::search` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::{base_of, modified_secs};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::safe_path;
use crate::state::AppState;

// --- /files/grep -------------------------------------------------------------

/// Query parameters for `GET /files/grep`: a literal/regex search of the workspace.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct GrepQuery {
    query: String,
    path: Option<String>,
    regex: Option<bool>,
    case_insensitive: Option<bool>,
    include: Option<String>,
    max_results: Option<usize>,
}

/// Grep the workspace for a literal or regex query.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace or the
/// regex is invalid, and [`ApiError::NotFound`] if the search path is missing.
#[utoipa::path(
    get,
    path = "/files/grep",
    tag = "files",
    params(GrepQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Search matches", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Search path not found", body = shared::ErrorResponse)
    )
)]
pub async fn grep(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GrepQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let path = q.path.as_deref().unwrap_or(".");
    let resolved = safe_path(path, &base)?;
    if !resolved.exists() {
        return Err(ApiError::NotFound("Search path not found".to_string()));
    }
    let max_results = q.max_results.unwrap_or(50).clamp(1, 500);
    // regex=True compiles the query; regex=False compiles re.escape(query).
    let pattern_src = if q.regex.unwrap_or(true) {
        q.query.clone()
    } else {
        regex::escape(&q.query)
    };
    let re = regex::RegexBuilder::new(&pattern_src)
        .case_insensitive(q.case_insensitive.unwrap_or(false))
        .build()
        .map_err(|e| ApiError::BadRequest(format!("Invalid regex: {e}")))?;
    let mut matches_arr: Vec<serde_json::Value> = Vec::new();
    let include = q.include.as_deref().map(|s| vec![s.to_string()]);
    for fpath in walk_files(&resolved, include.as_deref()) {
        // Lossy UTF-8 read (errors="replace"); a read failure (unreadable) is skipped.
        let Ok(fbytes) = std::fs::read(&fpath) else {
            continue;
        };
        let content = String::from_utf8_lossy(&fbytes);
        for (idx, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches_arr.push(serde_json::json!({
                    "file": fpath,
                    "line": idx + 1,
                    "content": line,
                }));
                if matches_arr.len() >= max_results {
                    return Ok(Json(serde_json::json!({
                        "query": q.query,
                        "path": resolved,
                        "matches": matches_arr,
                        "truncated": true,
                    })));
                }
            }
        }
    }
    Ok(Json(serde_json::json!({
        "query": q.query,
        "path": resolved,
        "matches": matches_arr,
        "truncated": false,
    })))
}

/// All regular files under `root` (sorted); optional fnmatch include filter.
/// If `root` is a file, returns `[root]`.
fn walk_files(root: &Path, include: Option<&[String]>) -> Vec<PathBuf> {
    let Ok(meta) = std::fs::metadata(root) else {
        return Vec::new();
    };
    if meta.is_file() {
        return vec![root.to_path_buf()];
    }
    if !meta.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_files(root, include, &mut out);
    out.sort();
    out
}

fn collect_files(dir: &Path, include: Option<&[String]>, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        // os.path.isdir follows symlinks; metadata failure → treat as non-dir.
        let is_dir = std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
        if is_dir {
            collect_files(&path, include, out);
        } else {
            if let Some(pats) = include {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !pats.iter().any(|p| fnmatch(name, p)) {
                    continue;
                }
            }
            out.push(path);
        }
    }
}

// --- /files/glob -------------------------------------------------------------

/// Query parameters for `GET /files/glob`: match entries by pattern and optional type.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct GlobQuery {
    pattern: String,
    path: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    max_results: Option<usize>,
}

/// Glob-match workspace entries by pattern and optional type.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] if the path escapes the workspace and
/// [`ApiError::NotFound`] if the search directory is missing.
#[utoipa::path(
    get,
    path = "/files/glob",
    tag = "files",
    params(GlobQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Glob matches", body = serde_json::Value),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Search directory not found", body = shared::ErrorResponse)
    )
)]
pub async fn glob_search(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GlobQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let path = q.path.as_deref().unwrap_or(".");
    let resolved = safe_path(path, &base)?;
    if !resolved.exists() {
        return Err(ApiError::NotFound("Search directory not found".to_string()));
    }
    let kind = q.kind.as_deref().unwrap_or("any");
    let max_results = q.max_results.unwrap_or(50).clamp(1, 500);
    // Collect all candidates (walked like os.walk), then sort by path; truncated
    // iff total >= max_results (append-then-check short-circuit).
    let mut found: Vec<(String, bool, u64, f64)> = Vec::new();
    glob_collect(&resolved, &resolved, &q.pattern, kind, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = found.len() >= max_results;
    let matches_arr: Vec<serde_json::Value> = found
        .into_iter()
        .take(max_results)
        .map(|(path, is_dir, size, modified)| {
            serde_json::json!({
                "path": path,
                "type": if is_dir { "directory" } else { "file" },
                "size": size,
                "modified": modified,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "pattern": q.pattern,
        "path": resolved,
        "matches": matches_arr,
        "truncated": truncated,
    })))
}

/// Walk `dir` like os.walk, pushing matching entries (relpath, `is_dir`, size, mtime).
fn glob_collect(
    root: &Path,
    dir: &Path,
    pattern: &str,
    kind: &str,
    out: &mut Vec<(String, bool, u64, f64)>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let is_dir = std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if fnmatch(&rel, pattern) || fnmatch(&name, pattern) {
            if kind == "file" && is_dir {
                // type filter excludes this entry (do not push).
            } else if kind == "directory" && !is_dir {
                // type filter excludes this entry.
            } else if let Ok(st) = std::fs::metadata(&path) {
                // os.stat failure (broken symlink) → skip.
                out.push((rel, is_dir, st.len(), modified_secs(&st)));
            }
        }
        if is_dir {
            subdirs.push(path);
        }
    }
    for d in subdirs {
        glob_collect(root, &d, pattern, kind, out);
    }
}

/// Shell-style fnmatch (`fnmatch.fnmatch`, case-sensitive on Linux):
/// translates `*`→`.*`, `?`→`.`, `[...]`→char class, anchors the whole string.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let re_src = fnmatch_translate(pattern);
    regex::Regex::new(&re_src).is_ok_and(|re| re.is_match(name))
}

/// Translate a shell glob into an anchored regex
/// (`fnmatch.translate`, classic form). Returns `^...$`.
//
// reason: char-by-char port of CPython's `fnmatch.translate`; the single-letter
// names (`c` char, `i`/`j`/`n` indices, `s` builder, `sc` char iterator) mirror
// the reference algorithm and aid side-by-side review.
#[allow(clippy::many_single_char_names)]
fn fnmatch_translate(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut res = String::new();
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => res.push_str(".*"),
            '?' => res.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    res.push_str("\\[");
                } else {
                    let stuff: String = chars[i..j].iter().collect();
                    i = j + 1;
                    let mut s = String::new();
                    let mut sc = stuff.chars();
                    match sc.next() {
                        Some('!') => {
                            s.push('^');
                            s.push_str(sc.as_str());
                        }
                        Some('^') => {
                            s.push_str("\\^");
                            s.push_str(sc.as_str());
                        }
                        other => {
                            if let Some(first) = other {
                                s.push(first);
                            }
                            s.push_str(sc.as_str());
                        }
                    }
                    res.push('[');
                    res.push_str(&s);
                    res.push(']');
                }
            }
            _ => {
                if "\\.+()|^${}".contains(c) {
                    res.push('\\');
                }
                res.push(c);
            }
        }
    }
    format!("^{res}$")
}

// --- /files/search + /files/matches (open-terminal 0.11.36 / 0.12.0) ------------

/// Upstream `MATCH_PAGE_SIZE`.
const MATCH_PAGE_SIZE: usize = 100;
/// Upstream `MAX_CONTENT_MATCHES_PER_FILE`.
const MAX_CONTENT_MATCHES_PER_FILE: usize = 3;
/// Upstream `MAX_CONTENT_SEARCH_FILE_SIZE` (1 MiB).
const MAX_CONTENT_SEARCH_FILE_SIZE: u64 = 1024 * 1024;

/// Which candidate kinds a search collects (`type` query param).
#[derive(Clone, Copy, PartialEq)]
enum WantKind {
    Any,
    File,
    Directory,
}

impl WantKind {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        match raw {
            None | Some("any") => Ok(Self::Any),
            Some("file") => Ok(Self::File),
            Some("directory") => Ok(Self::Directory),
            Some(other) => Err(ApiError::BadRequest(format!(
                "type must be one of file|directory|any, got {other}"
            ))),
        }
    }

    fn wants(self, is_dir: bool) -> bool {
        match self {
            Self::Any => true,
            Self::File => !is_dir,
            Self::Directory => is_dir,
        }
    }
}

fn is_hidden_rel(rel: &str) -> bool {
    rel.replace('\\', "/")
        .split('/')
        .any(|p| !p.is_empty() && p.starts_with('.'))
}

/// Candidate walk mirroring upstream: `git ls-files -co --exclude-standard`
/// when the workspace is a git repo (gitignore honored, parent dirs of
/// tracked files added as directory candidates), plain recursive walk
/// otherwise. Returns `(absolute path, is_dir)` pairs.
fn walk_candidates(target: &Path, show_hidden: bool, want: WantKind) -> Vec<(PathBuf, bool)> {
    if let Some(cands) = git_candidates(target, show_hidden, want) {
        return cands;
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(target) {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        // Skip hidden entries (and everything inside them) unless opted in —
        // matches upstream's pruned os.walk.
        if !show_hidden && entry_hidden(&entry, target) {
            continue;
        }
        let is_dir = entry.file_type().is_dir();
        if want.wants(is_dir) {
            out.push((entry.path().to_path_buf(), is_dir));
        }
    }
    out
}

/// Whether the entry sits inside (or is) a dotfile/dot-directory.
fn entry_hidden(entry: &walkdir::DirEntry, target: &Path) -> bool {
    entry.path().strip_prefix(target).is_ok_and(|rel| {
        rel.components()
            .filter_map(|c| c.as_os_str().to_str())
            .any(|p| p.starts_with('.'))
    })
}

fn git_candidates(
    target: &Path,
    show_hidden: bool,
    want: WantKind,
) -> Option<Vec<(PathBuf, bool)>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["ls-files", "-co", "--exclude-standard", "-z", "--", "."])
        .output()
        .ok()?;
    // Non-repo (or no git): fall back to the plain walk.
    if !out.status.success() {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let mut cands: Vec<(PathBuf, bool)> = Vec::new();
    for raw in out.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(raw).into_owned();
        if !show_hidden && is_hidden_rel(&rel) {
            continue;
        }
        let full = target.join(&rel);
        // git ls-files -co lists files (and, with -o, untracked ones);
        // everything it prints is a file.
        if want.wants(false) && seen.insert(full.clone()) {
            cands.push((full.clone(), false));
        }
        // Parent directories of listed files become directory candidates.
        let mut parent = Path::new(&rel).parent();
        while let Some(p) = parent {
            if p.as_os_str().is_empty() {
                break;
            }
            let prel = p.to_string_lossy().into_owned();
            if show_hidden || !is_hidden_rel(&prel) {
                let d = target.join(p);
                if want.wants(true) && seen.insert(d.clone()) {
                    cands.push((d, true));
                }
            }
            parent = p.parent();
        }
    }
    Some(cands)
}

/// Query parameters for `GET /files/search` (open-terminal 0.11.36).
#[derive(Deserialize, utoipa::IntoParams)]
pub struct SearchQuery {
    /// Filename search term.
    pub query: Option<String>,
    /// Directory to search within (default `.`).
    pub path: Option<String>,
    /// Maximum number of matches (1..=100, default 20).
    pub limit: Option<usize>,
    /// Type filter: `file`, `directory`, or `any` (default `any`).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Include hidden dotfiles and dot-directories.
    pub show_hidden: Option<bool>,
}

/// One ranked result row (`path` is absolute, matching upstream).
#[derive(Serialize, utoipa::ToSchema)]
pub struct SearchResult {
    /// Absolute path of the match (upstream returns absolute paths).
    pub path: String,
    /// Entry base name.
    pub name: String,
    /// `file` or `directory`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Size in bytes.
    pub size: u64,
    /// mtime in seconds since epoch.
    pub modified: f64,
}

/// Response of `GET /files/search`.
#[derive(Serialize, utoipa::ToSchema)]
pub struct SearchResponse {
    /// Ranked matches, at most `limit`.
    pub results: Vec<SearchResult>,
}

/// Search files and subdirectories by ranked filename match (0.11.36).
///
/// Rank 0 = exact name (case-insensitive), 1 = name prefix, 2 = substring;
/// an empty `query` matches everything at rank 2. Sort key
/// `(rank, name.len(), relative path lowercase)` — the upstream order.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for an out-of-range `limit` or bad
/// `type`, and [`ApiError::NotFound`] when `path` is missing or not a directory.
#[utoipa::path(
    get,
    path = "/files/search",
    tag = "files",
    params(SearchQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Ranked filename matches", body = SearchResponse),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Search directory not found", body = shared::ErrorResponse)
    )
)]
pub async fn search_files(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, ApiError> {
    let limit = q.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be 1..=100".to_string()));
    }
    let want = WantKind::parse(q.kind.as_deref())?;
    let raw = q.path.as_deref().unwrap_or(".");
    let base = base_of(&state, &headers)?;
    let target = safe_path(raw, &base)?;
    if !target.is_dir() {
        return Err(ApiError::NotFound("Search directory not found".to_string()));
    }
    let query_lower = q.query.as_deref().unwrap_or("").trim().to_lowercase();
    let mut ranked: Vec<(u8, usize, String, SearchResult)> = Vec::new();
    for (full, is_dir) in walk_candidates(&target, q.show_hidden.unwrap_or(false), want) {
        let Some(name) = full.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name_lower = name.to_lowercase();
        let rank = if query_lower.is_empty() {
            2
        } else if name_lower == query_lower {
            0
        } else if name_lower.starts_with(&query_lower) {
            1
        } else if name_lower.contains(&query_lower) {
            2
        } else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&full) else {
            continue;
        };
        let rel = full
            .strip_prefix(&target)
            .unwrap_or(&full)
            .to_string_lossy()
            .to_lowercase();
        ranked.push((
            rank,
            name.len(),
            rel,
            SearchResult {
                path: full.to_string_lossy().into_owned(),
                name: name.to_string(),
                kind: if is_dir { "directory" } else { "file" },
                size: meta.len(),
                modified: modified_secs(&meta),
            },
        ));
    }
    ranked.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let results = ranked.into_iter().take(limit).map(|r| r.3).collect();
    Ok(Json(SearchResponse { results }))
}

// --- /files/matches (open-terminal 0.12.0) ------------------------------------

/// Query parameters for `GET /files/matches`.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct MatchesQuery {
    /// Literal text to match (required, non-blank).
    pub query: String,
    /// Directory to search within (default `.`).
    pub path: Option<String>,
    /// Include hidden dotfiles and dot-directories.
    pub show_hidden: Option<bool>,
    /// Result offset (default 0).
    pub offset: Option<usize>,
    /// Maximum number of results (1..=100, default 100).
    pub limit: Option<usize>,
}

/// One content match inside a file: line, UTF-16 code-unit column, line text.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ContentMatch {
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column in UTF-16 code units (upstream mirror).
    pub column: usize,
    /// The matched line, trimmed of its line terminator.
    pub text: String,
}

/// One unified-search row (name match, content matches, or both).
#[derive(Serialize, utoipa::ToSchema)]
pub struct MatchResult {
    /// Absolute path of the match.
    pub path: String,
    /// Path relative to the search directory (`/`-separated).
    pub relative_path: String,
    /// Entry base name.
    pub name: String,
    /// `file` or `directory`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Whether the name/path matched (score 0-3).
    pub name_match: bool,
    /// Literal content matches inside the file (<= 3).
    pub content_matches: Vec<ContentMatch>,
}

/// Response of `GET /files/matches`.
#[derive(Serialize, utoipa::ToSchema)]
pub struct MatchesResponse {
    /// This page of matches.
    pub results: Vec<MatchResult>,
    /// Offset of the next page, or `None` when exhausted.
    pub next_offset: Option<usize>,
}

/// Case-insensitive literal content scan of one file, mirroring upstream's
/// portable (non-rg) path: skip >1 MiB files and NUL-sniffed binaries, ≤3
/// matches per file, UTF-16 columns.
fn content_matches(full: &Path, query_lower: &str) -> Vec<ContentMatch> {
    let Ok(meta) = std::fs::symlink_metadata(full) else {
        return Vec::new();
    };
    if meta.is_symlink() || meta.len() > MAX_CONTENT_SEARCH_FILE_SIZE {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(full) else {
        return Vec::new();
    };
    if bytes.get(..8192).is_some_and(|w| w.contains(&0)) {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut out = Vec::new();
    for (idx, raw_line) in text.split_inclusive('\n').enumerate() {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let lowered = line.to_lowercase();
        let Some(at) = lowered.find(query_lower) else {
            continue;
        };
        out.push(ContentMatch {
            line: idx + 1,
            column: lowered[..at].encode_utf16().count() + 1,
            text: line.to_string(),
        });
        if out.len() >= MAX_CONTENT_MATCHES_PER_FILE {
            break;
        }
    }
    out
}

/// Unified search by ranked name, path, and content (0.12.0).
///
/// Score 0 = exact name, 1 = name prefix, 2 = name contains,
/// 3 = relative path contains, 4 = content-only. Sorted by
/// `(score, relative_path.len(), relative path lowercase)` and paginated
/// with `next_offset`.
///
/// # Errors
///
/// Returns [`ApiError::BadRequest`] for a blank query or out-of-range
/// `offset`/`limit`, and [`ApiError::NotFound`] when `path` is missing or
/// not a directory.
#[utoipa::path(
    get,
    path = "/files/matches",
    tag = "files",
    params(MatchesQuery),
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Unified matches page", body = MatchesResponse),
        (status = 400, body = shared::ErrorResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 404, description = "Search directory not found", body = shared::ErrorResponse)
    )
)]
pub async fn match_files(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MatchesQuery>,
) -> Result<Json<MatchesResponse>, ApiError> {
    let query = q.query.trim().to_string();
    if query.is_empty() {
        return Err(ApiError::BadRequest("Query must not be blank".to_string()));
    }
    let limit = q.limit.unwrap_or(MATCH_PAGE_SIZE);
    if !(1..=MATCH_PAGE_SIZE).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be 1..=100".to_string()));
    }
    let offset = q.offset.unwrap_or(0);
    let raw = q.path.as_deref().unwrap_or(".");
    let base = base_of(&state, &headers)?;
    let target = safe_path(raw, &base)?;
    if !target.is_dir() {
        return Err(ApiError::NotFound("Search directory not found".to_string()));
    }
    let query_lower = query.to_lowercase();
    let mut rows: Vec<(u8, usize, String, MatchResult)> = Vec::new();
    for (full, is_dir) in walk_candidates(&target, q.show_hidden.unwrap_or(false), WantKind::Any) {
        let Some(name) = full.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = full
            .strip_prefix(&target)
            .unwrap_or(&full)
            .to_string_lossy()
            .replace('\\', "/");
        let name_lower = name.to_lowercase();
        let rel_lower = rel.to_lowercase();
        let score = if name_lower == query_lower {
            0
        } else if name_lower.starts_with(&query_lower) {
            1
        } else if name_lower.contains(&query_lower) {
            2
        } else if rel_lower.contains(&query_lower) {
            3
        } else {
            4
        };
        let hits = if is_dir {
            Vec::new()
        } else {
            content_matches(&full, &query_lower)
        };
        let name_match = score < 4;
        if !name_match && hits.is_empty() {
            continue;
        }
        rows.push((
            score,
            rel.len(),
            rel_lower.clone(),
            MatchResult {
                path: full.to_string_lossy().into_owned(),
                relative_path: rel,
                name: name.to_string(),
                kind: if is_dir { "directory" } else { "file" },
                name_match,
                content_matches: hits,
            },
        ));
    }
    rows.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let total = rows.len();
    let results = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|r| r.3)
        .collect::<Vec<_>>();
    let next_offset = if offset + limit < total {
        Some(offset + limit)
    } else {
        None
    };
    Ok(Json(MatchesResponse {
        results,
        next_offset,
    }))
}
