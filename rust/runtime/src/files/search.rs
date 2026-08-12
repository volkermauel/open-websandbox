//! `files::search` — filesystem handlers, split out of the former `files.rs` (#102 D1).
use super::{base_of, modified_secs};
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::safe_path::safe_path;
use crate::state::AppState;

// --- /files/grep -------------------------------------------------------------

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
