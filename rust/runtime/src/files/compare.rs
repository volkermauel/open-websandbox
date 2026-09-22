//! `POST /files/compare` — read-only text comparison (open-terminal `main`
//! post-v0.12.3, driven by OWUI v0.11.4; issue #195).
//!
//! Upstream shells the diff out to a Python worker with LibreOffice-backed
//! document extraction. We compute the same result shape in-process via
//! [`similar`](https://docs.rs/similar) — and, per our documented `/files/read`
//! divergence, do **no** Office/PDF extraction: non-text binaries come back
//! 422 just like an upstream worker failure would.

#![forbid(unsafe_code)]

use std::path::Path;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Upstream `MAX_FILE_BYTES`: 50 MiB per file.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Upstream `MAX_TEXT_CHARS`: 2 million decoded characters.
const MAX_TEXT_CHARS: usize = 2_000_000;
/// Upstream `MAX_LINES`: 50,000 lines.
const MAX_LINES: usize = 50_000;
/// Upstream `segments()` bound: intraline matching only for lines ≤ 4096 chars.
const SEGMENT_LINE_MAX: usize = 4096;

/// Request body for `POST /files/compare`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct CompareRequest {
    /// Workspace-relative path to the original file.
    pub original: String,
    /// Workspace-relative path to the revised file.
    pub revised: String,
    /// Compare with intra-line whitespace removed (default `false`).
    #[serde(default)]
    pub ignore_whitespace: bool,
}

/// One intraline segment: a run of text, marked changed or unchanged.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Segment {
    /// The text run.
    pub text: String,
    /// Whether this run differs between the two sides.
    pub changed: bool,
}

/// One output line inside a hunk (context / removed / added).
#[derive(Serialize, utoipa::ToSchema)]
pub struct CompareLine {
    /// `context` | `removed` | `added`.
    #[serde(rename = "type")]
    pub kind: String,
    /// 1-based line number on the original side (`null` for added lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_number: Option<u64>,
    /// 1-based line number on the revised side (`null` for removed lines).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_number: Option<u64>,
    /// Line content on the original side.
    pub content: String,
    /// Line content on the revised side (context lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_content: Option<String>,
    /// Intraline segmentation (paired removed/added lines only).
    pub segments: Vec<Segment>,
}

/// One hunk: a `@@ -a,b +c,d @@` header plus its lines.
#[derive(Serialize, utoipa::ToSchema)]
pub struct CompareHunk {
    /// `@@ -{old_start},{old_len} +{new_start},{new_len} @@`
    pub header: String,
    /// The hunk's lines, removed-before-added within each change block.
    pub lines: Vec<CompareLine>,
}

/// File summary embedded in the comparison result.
#[derive(Serialize, utoipa::ToSchema)]
pub struct CompareFile {
    /// Absolute path of the compared file.
    pub path: String,
    /// Basename.
    pub name: String,
    /// Extraction notices (always empty for plain text).
    pub notices: Vec<String>,
}

/// `POST /files/compare` response (upstream `compare()` result shape).
#[derive(Serialize, utoipa::ToSchema)]
pub struct CompareResponse {
    /// Original-side summary.
    pub original: CompareFile,
    /// Revised-side summary.
    pub revised: CompareFile,
    /// Total added lines.
    pub additions: u64,
    /// Total removed lines.
    pub deletions: u64,
    /// Change hunks (3 context lines, difflib-style).
    pub hunks: Vec<CompareHunk>,
}

/// Decode one side: resolve + confine the path, then decode as text with BOM
/// detection (upstream `extract()` minus the document-extraction branch).
fn extract_side(base: &Path, raw: &str) -> Result<(CompareFile, Vec<String>), ApiError> {
    let name = Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw)
        .to_string();
    let resolved = crate::safe_path::safe_path(raw, base)?;
    let meta = std::fs::metadata(&resolved)
        .map_err(|_| ApiError::UnprocessableEntity(format!("{name}: File not found.")))?;
    if !meta.is_file() {
        return Err(ApiError::UnprocessableEntity(format!(
            "{name}: File not found."
        )));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ApiError::UnprocessableEntity(format!(
            "{name}: File exceeds the 50 MiB comparison limit."
        )));
    }
    let bytes = std::fs::read(&resolved)
        .map_err(|e| ApiError::UnprocessableEntity(format!("{name}: {e}")))?;
    let text =
        decode_text(&bytes).map_err(|e| ApiError::UnprocessableEntity(format!("{name}: {e}")))?;
    if text
        .chars()
        .any(|c| c < ' ' && !matches!(c, '\n' | '\r' | '\t' | '\u{c}'))
    {
        return Err(ApiError::UnprocessableEntity(format!(
            "{name}: Unsupported binary file."
        )));
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    if text.chars().count() > MAX_TEXT_CHARS || lines.len() > MAX_LINES {
        return Err(ApiError::UnprocessableEntity(format!(
            "{name}: Extracted text exceeds the comparison limit (2 million characters / 50,000 lines). Nothing was truncated."
        )));
    }
    Ok((
        CompareFile {
            path: resolved.to_string_lossy().to_string(),
            name,
            notices: Vec::new(),
        },
        lines,
    ))
}

/// BOM-aware decode: UTF-32/UTF-16 BOMs, UTF-8 BOM (stripped), else UTF-8.
fn decode_text(raw: &[u8]) -> Result<String, String> {
    let bom32le: [u8; 4] = [0xff, 0xfe, 0x00, 0x00];
    let bom32be: [u8; 4] = [0x00, 0x00, 0xfe, 0xff];
    if raw.starts_with(&bom32le) || raw.starts_with(&bom32be) {
        let units = decode_utf32(raw)?;
        let chars: Result<Vec<char>, _> = units
            .iter()
            .map(|&u| char::from_u32(u).ok_or("Unsupported binary file or text encoding."))
            .collect();
        return chars
            .map(|c| c.into_iter().collect::<String>())
            .map_err(std::string::ToString::to_string);
    }
    if raw.starts_with(&[0xff, 0xfe]) || raw.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(raw)
            .map_err(|_| "Unsupported binary file or text encoding.".to_string());
    }
    String::from_utf8(
        raw.strip_prefix(&[0xef, 0xbb, 0xbf])
            .unwrap_or(raw)
            .to_vec(),
    )
    .map_err(|_| "Unsupported binary file or text encoding.".to_string())
}

fn decode_utf16(raw: &[u8]) -> Result<String, String> {
    let body = &raw[2..];
    let be = raw[1] == 0xff;
    let (chunks, _rest) = body.as_chunks::<2>();
    let units: Vec<u16> = chunks
        .iter()
        .map(|c| {
            if be {
                u16::from_be_bytes(*c)
            } else {
                u16::from_le_bytes(*c)
            }
        })
        .collect();
    String::from_utf16(&units).map_err(|_| "Unsupported binary file or text encoding.".to_string())
}

fn decode_utf32(raw: &[u8]) -> Result<Vec<u32>, String> {
    let body = &raw[4..];
    let be = raw[0] == 0x00;
    let units: Vec<u32> = body
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            if be {
                u32::from_be_bytes(*c)
            } else {
                u32::from_le_bytes(*c)
            }
        })
        .collect();
    units.iter().try_for_each(|&u| {
        char::from_u32(u)
            .map(|_| ())
            .ok_or("Unsupported binary file or text encoding.")
    })?;
    Ok(units)
}

/// Tokenize a line for intraline matching: `\w+ | [^\w\s] | \s+` (upstream).
fn tokenize(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        if is_word(c) {
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
        } else if c.is_ascii_whitespace() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        } else {
            i += 1;
        }
        out.push(&line[start..i]);
    }
    out
}

/// Upstream `segments()`: paired-token diff of one removed/added line pair.
/// Lines longer than 4096 chars get a single unchanged segment.
fn segments(original: &str, revised: &str) -> (Vec<Segment>, Vec<Segment>) {
    if original.chars().count() > SEGMENT_LINE_MAX || revised.chars().count() > SEGMENT_LINE_MAX {
        return (
            vec![Segment {
                text: original.to_string(),
                changed: false,
            }],
            vec![Segment {
                text: revised.to_string(),
                changed: false,
            }],
        );
    }
    let a = tokenize(original);
    let b = tokenize(revised);
    let diff = similar::TextDiff::from_slices(&a, &b);
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for op in diff.ops() {
        use similar::DiffOp;
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
                ..
            } => {
                for k in 0..*len {
                    left.push(Segment {
                        text: a[old_index + k].to_string(),
                        changed: false,
                    });
                    right.push(Segment {
                        text: b[new_index + k].to_string(),
                        changed: false,
                    });
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for k in 0..*old_len {
                    left.push(Segment {
                        text: a[old_index + k].to_string(),
                        changed: true,
                    });
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for k in 0..*new_len {
                    right.push(Segment {
                        text: b[new_index + k].to_string(),
                        changed: true,
                    });
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
                ..
            } => {
                for k in 0..*old_len {
                    left.push(Segment {
                        text: a[old_index + k].to_string(),
                        changed: true,
                    });
                }
                for k in 0..*new_len {
                    right.push(Segment {
                        text: b[new_index + k].to_string(),
                        changed: true,
                    });
                }
            }
        }
    }
    (left, right)
}

/// Whitespace-stripped comparison key for a line (upstream `keys()`).
fn key_of(line: &str, ignore_whitespace: bool) -> String {
    if ignore_whitespace {
        line.split_whitespace().collect::<String>()
    } else {
        line.to_string()
    }
}

/// Compute the comparison result over already-extracted sides.
#[allow(clippy::many_single_char_names, clippy::needless_pass_by_value)] // `a`/`b` + difflib `i j k l` header math mirror upstream
fn compare(
    a: Vec<String>,
    b: Vec<String>,
    ignore_whitespace: bool,
) -> (Vec<CompareHunk>, u64, u64) {
    let keys_a: Vec<String> = a.iter().map(|l| key_of(l, ignore_whitespace)).collect();
    let keys_b: Vec<String> = b.iter().map(|l| key_of(l, ignore_whitespace)).collect();
    let refs_a: Vec<&str> = keys_a.iter().map(String::as_str).collect();
    let refs_b: Vec<&str> = keys_b.iter().map(String::as_str).collect();
    let diff = similar::TextDiff::from_slices(&refs_a, &refs_b);
    let mut hunks = Vec::new();
    let mut additions = 0u64;
    let mut deletions = 0u64;
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        let first = &group[0];
        let last = group.last().expect("non-empty group");
        let (i, k) = (first.old_range().start, first.new_range().start);
        let (j, l) = (last.old_range().end, last.new_range().end);
        let old_start = if j > i { i + 1 } else { i };
        let new_start = if l > k { k + 1 } else { k };
        let mut lines = Vec::new();
        for op in &group {
            use similar::DiffOp;
            match op {
                DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                    ..
                } => {
                    for x in 0..*len {
                        let (ox, ny) = (old_index + x, new_index + x);
                        lines.push(CompareLine {
                            kind: "context".to_string(),
                            old_number: Some(ox as u64 + 1),
                            new_number: Some(ny as u64 + 1),
                            content: a[ox].clone(),
                            revised_content: Some(b[ny].clone()),
                            segments: vec![Segment {
                                text: a[ox].clone(),
                                changed: false,
                            }],
                        });
                    }
                }
                DiffOp::Delete {
                    old_index, old_len, ..
                } => {
                    for x in 0..*old_len {
                        let ox = old_index + x;
                        lines.push(CompareLine {
                            kind: "removed".to_string(),
                            old_number: Some(ox as u64 + 1),
                            new_number: None,
                            content: a[ox].clone(),
                            revised_content: None,
                            segments: vec![Segment {
                                text: a[ox].clone(),
                                changed: false,
                            }],
                        });
                        deletions += 1;
                    }
                }
                DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    for y in 0..*new_len {
                        let ny = new_index + y;
                        lines.push(CompareLine {
                            kind: "added".to_string(),
                            old_number: None,
                            new_number: Some(ny as u64 + 1),
                            content: b[ny].clone(),
                            revised_content: None,
                            segments: vec![Segment {
                                text: b[ny].clone(),
                                changed: false,
                            }],
                        });
                        additions += 1;
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                    ..
                } => {
                    let mut removed = Vec::new();
                    for x in 0..*old_len {
                        let ox = old_index + x;
                        removed.push((
                            CompareLine {
                                kind: "removed".to_string(),
                                old_number: Some(ox as u64 + 1),
                                new_number: None,
                                content: a[ox].clone(),
                                revised_content: None,
                                segments: Vec::new(),
                            },
                            a[ox].clone(),
                        ));
                        deletions += 1;
                    }
                    let mut added = Vec::new();
                    for y in 0..*new_len {
                        let ny = new_index + y;
                        added.push((
                            CompareLine {
                                kind: "added".to_string(),
                                old_number: None,
                                new_number: Some(ny as u64 + 1),
                                content: b[ny].clone(),
                                revised_content: None,
                                segments: Vec::new(),
                            },
                            b[ny].clone(),
                        ));
                        additions += 1;
                    }
                    // Pair removed/added lines positionally for intraline segments.
                    for (removed, added) in removed.iter_mut().zip(added.iter_mut()) {
                        let (ls, rs) = segments(&removed.1, &added.1);
                        removed.0.segments = ls;
                        added.0.segments = rs;
                    }
                    lines.extend(removed.into_iter().map(|(l, _)| l));
                    lines.extend(added.into_iter().map(|(l, _)| l));
                }
            }
        }
        hunks.push(CompareHunk {
            header: format!("@@ -{old_start},{} +{new_start},{} @@", j - i, l - k),
            lines,
        });
    }
    (hunks, additions, deletions)
}

/// `POST /files/compare` — read-only text diff of two workspace files.
///
/// # Errors
///
/// Returns [`ApiError::UnprocessableEntity`] (422, upstream worker-error parity)
/// for missing files, oversized/binary inputs and path escapes;
/// [`ApiError::BadRequest`] if the body cannot be parsed.
#[utoipa::path(
    post,
    path = "/files/compare",
    tag = "files",
    request_body = CompareRequest,
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Structured diff", body = CompareResponse),
        (status = 401, body = shared::ErrorResponse),
        (status = 422, description = "Missing / oversized / binary file", body = shared::ErrorResponse)
    )
)]
pub async fn compare_files(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CompareRequest>,
) -> Result<Json<CompareResponse>, ApiError> {
    let base = super::base_of(&state, &headers)?;
    // Offload the (potentially 50 k-line) diff to the blocking pool.
    let (original, a) = extract_side(&base, &req.original)?;
    let (revised, b) = extract_side(&base, &req.revised)?;
    let ignore = req.ignore_whitespace;
    let (hunks, additions, deletions) = tokio::task::spawn_blocking(move || compare(a, b, ignore))
        .await
        .map_err(|e| ApiError::Internal(format!("compare task failed: {e}")))?;
    Ok(Json(CompareResponse {
        original,
        revised,
        additions,
        deletions,
        hunks,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_have_no_hunks() {
        let a = vec!["one".to_string(), "two".to_string()];
        let (hunks, add, del) = compare(a.clone(), a, false);
        assert!(hunks.is_empty());
        assert_eq!((add, del), (0, 0));
    }

    #[test]
    fn simple_change_pairs_segments() {
        let a = vec!["hello world".to_string()];
        let b = vec!["hello brave world".to_string()];
        let (hunks, add, del) = compare(a, b, false);
        assert_eq!((add, del), (1, 1));
        assert_eq!(hunks.len(), 1);
        let kinds: Vec<&str> = hunks[0].lines.iter().map(|l| l.kind.as_str()).collect();
        assert_eq!(kinds, vec!["removed", "added"]);
        let added = &hunks[0].lines[1];
        // "brave " is the changed segment on the added side.
        assert!(added
            .segments
            .iter()
            .any(|s| s.changed && s.text.trim() == "brave"));
    }

    #[test]
    fn header_uses_difflib_start_convention() {
        let a: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
        let mut b = a.clone();
        b[5] = "changed".to_string();
        let (hunks, _, _) = compare(a, b, false);
        assert_eq!(hunks.len(), 1);
        // Change at index 5 with 3 context lines: @@ -3,7 +3,7 @@
        assert!(
            hunks[0].header.starts_with("@@ -3,7 +3,7 @@"),
            "got {}",
            hunks[0].header
        );
    }

    #[test]
    fn decode_strips_utf8_bom() {
        let raw = [0xef, 0xbb, 0xbf, b'h', b'i'];
        assert_eq!(decode_text(&raw).unwrap(), "hi");
    }
}
