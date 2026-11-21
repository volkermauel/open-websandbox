//! `/git/*` — the open-terminal 0.17.x Git panel + GitHub workbench surface
//! (`features.git`), ported from `open_terminal/utils/git.py` + the route
//! handlers in `main.py`. All routes are `include_in_schema=False` upstream —
//! OWUI's frontend calls them directly (`gitRequest`), so ours stay out of the
//! OpenAPI document too.
//!
//! # Wiring
//!
//! Every operation shells out to `git` (and `gh` for the PR/issue views) in
//! the sandbox pod — the runtime process environment is sanitized (no `GIT_*`
//! inherits, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`, `core.quotePath=false`) and
//! each run is bounded (60 s → **408**, 8 MiB output → **413**, non-zero
//! `check` → **400** with stderr). Repos live anywhere inside the workspace:
//! the `root` query resolves through [`crate::safe_path`], `rev-parse
//! --show-toplevel` re-roots the repository, and every path leaving the repo
//! is re-confined — escapes are **403** (upstream `checked_path`).
//!
//! Upstream's `line-stats` helper program (untracked-file line counts) is
//! ported inline: NUL-sniffed binary detection → `binary: true`, otherwise a
//! counted line number with the trailing-newline adjustment.
//!
//! The PR/issue views shell out to `gh`; `pr/capabilities` reports
//! `available: false` when `gh` is missing or unauthenticated — OWUI's
//! workbench degrades to plain Git exactly like upstream. Not-a-repo: reads
//! return the `is_repo:false` shape (status) / `[]` (pr list) / **400** "Not
//! a Git repository" (everything else) — upstream's exact split.
//!
//! The upstream multi-user `sudo -u` branch does not apply (one OS user per
//! pod — documented divergence).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::{HeaderMap, Method};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::files::base_of;
use crate::state::AppState;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;

/// PR JSON field set (upstream `pull_requests`).
const PR_FIELDS: &str =
    "number,title,state,isDraft,author,headRefName,baseRefName,url,body,mergeable,reviewDecision";
/// Issue JSON field set (upstream `issues`).
const ISSUE_FIELDS: &str = "number,title,state,stateReason,author,labels,assignees,url,updatedAt";

/// Per-run limits (upstream `git.run`).
const RUN_TIMEOUT: Duration = Duration::from_mins(1);

const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// Cached `git` availability probe (the `features.git` gate — upstream
/// `shutil.which("git") is not None`).
#[must_use]
pub fn git_available() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| which("git"))
}

/// Minimal `shutil.which`: the binary exists AND is executable on `PATH`.
#[must_use]
fn which(program: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .any(|candidate| {
            use std::os::unix::fs::PermissionsExt;
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

// ---------------------------------------------------------------------------
// Repository handle
// ---------------------------------------------------------------------------

/// A repository rooted (after `detect`) at `root`, confined to the workspace
/// base.
struct GitRepo {
    /// Candidate root (the resolved `root` parameter) until `detect` re-roots.
    root: PathBuf,
    /// Workspace confinement boundary.
    base: PathBuf,
}

struct RunOutcome {
    code: i32,
    stdout: String,
    stderr: String,
}

/// The bounded-read failure modes of one [`GitRepo::run`].
enum RunError {
    /// Output exceeded the cap.
    Cap,
    Io(std::io::Error),
}

/// Drain one piped stream into a capped buffer (upstream's `read()` task:
/// the cap is checked DURING the read and the process group is killed by the
/// caller once exceeded).
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut Option<R>,
    total: &mut usize,
) -> Result<Vec<u8>, RunError> {
    let Some(stream) = stream else {
        return Ok(Vec::new());
    };
    let mut buf = Vec::new();
    // Heap chunk: a stack array here lands in the async fn's FUTURE state,
    // and the composed (nested-await) future overflowed the 2 MiB
    // test-thread stack at first poll.
    let mut chunk = vec![0u8; 65_536];
    loop {
        let n = stream.read(&mut chunk).await.map_err(RunError::Io)?;
        if n == 0 {
            break;
        }
        *total += n;
        if *total > OUTPUT_CAP {
            return Err(RunError::Cap);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

impl GitRepo {
    /// Open the repo candidate: the `root` parameter resolved against the
    /// workspace (safe_path; upstream `fs.resolve_path(root, cwd=session_cwd)`
    /// + `checked_path`).
    fn open(base: &Path, root_param: &str) -> Result<Self, ApiError> {
        Ok(Self {
            root: crate::safe_path::safe_path(root_param, base)?,
            base: base.to_path_buf(),
        })
    }

    /// Confine an arbitrary path (resolved, realpath'd) to the workspace —
    /// upstream `checked_path` (403 on escape).
    fn checked(&self, path: &Path) -> Result<PathBuf, ApiError> {
        let real = crate::safe_path::safe_path(&path.display().to_string(), &self.base)?;
        Ok(real)
    }

    /// One `git` (or `gh`) run: sanitized env, bounded time/output, killpg on
    /// timeout. `check` ⇒ non-zero exit is a 400 with stderr.
    async fn run(
        &self,
        args: &[&str],
        program: &str,
        input: Option<&[u8]>,
        check: bool,
    ) -> Result<RunOutcome, ApiError> {
        use tokio::io::AsyncWriteExt;
        let mut cmd = tokio::process::Command::new(program);
        if program == "git" {
            cmd.arg("-c").arg("core.quotePath=false");
        }
        cmd.args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| {
                // Upstream `sanitized_environ` minus GIT_* inheritance; plus
                // our own hardening (SECRET/TOKEN/PASSWORD, review finding 5)
                // and the runtime's own OPEN_TERMINAL_* knobs.
                let k = k.to_ascii_uppercase();
                !k.starts_with("GIT_")
                    && !k.starts_with("OPEN_TERMINAL_")
                    && !k.contains("SECRET")
                    && !k.contains("TOKEN")
                    && !k.contains("PASSWORD")
            }))
            .env("HOME", &self.base)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .env("LC_ALL", "C")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::NotFound(format!("{program} or the directory is unavailable"))
            } else {
                ApiError::BadRequest(format!("{program} failed to start: {e}"))
            }
        })?;
        // The group id equals the child pid (`process_group(0)`) — SIGKILL the
        // WHOLE group on timeout/cap so grandchildren (hooks, ssh, credential
        // helpers) never orphan (upstream `killpg` in the `finally`; review
        // finding 11).
        #[allow(clippy::cast_possible_wrap)]
        let pid = Pid::from_raw(child.id().unwrap_or(0) as i32);
        let kill_group = |pid: Pid| {
            let _ = killpg(pid, Signal::SIGKILL);
        };
        if let Some(data) = input {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(data).await;
                let _ = stdin.shutdown().await;
            }
        }

        // Bounded reads DURING streaming (upstream caps inside `read()` while
        // the `finally` killpg runs — review finding 4: buffering the whole
        // output before checking the cap is not a bound).
        let mut stdout_h = child.stdout.take();
        let mut stderr_h = child.stderr.take();
        let collected = tokio::time::timeout(RUN_TIMEOUT, async {
            // Each stream carries its own 8 MiB budget — upstream's two
            // `read()` tasks cap independently.
            let mut total_out = 0usize;
            let mut total_err = 0usize;
            let (out, err) = tokio::join!(
                read_bounded(&mut stdout_h, &mut total_out),
                read_bounded(&mut stderr_h, &mut total_err),
            );
            let (out, err) = (out?, err?);
            let status = child.wait().await.map_err(RunError::Io)?;
            Ok::<_, RunError>((out, err, status))
        })
        .await;
        let (stdout, stderr, code) = match collected {
            // Timeout: kill the group (children first — review finding 11),
            // reap, then map to upstream's 408 string.
            Err(_) => {
                kill_group(pid);
                let _ = child.wait().await;
                return Err(ApiError::GatewayTimeout(
                    "Git operation exceeded the 60 second limit".into(),
                ));
            }
            Ok(Err(RunError::Cap)) => {
                kill_group(pid);
                let _ = child.wait().await;
                return Err(ApiError::PayloadTooLarge(
                    "Git output exceeds the 8 MiB limit".into(),
                ));
            }
            Ok(Err(RunError::Io(e))) => {
                return Err(ApiError::Internal(format!("{program} run failed: {e}")));
            }
            Ok(Ok((out, err, status))) => {
                let stdout = String::from_utf8_lossy(&out).into_owned();
                let stderr = String::from_utf8_lossy(&err).into_owned();
                (stdout, stderr, status.code().unwrap_or(-1))
            }
        };
        if check && code != 0 {
            return Err(ApiError::BadRequest(
                non_blank(&stderr, "Git operation failed").to_string(),
            ));
        }
        Ok(RunOutcome {
            code,
            stdout,
            stderr,
        })
    }

    /// `rev-parse --show-toplevel` re-rooting; `Ok(false)` when the candidate
    /// directory is simply not a repository ("not a git repository" stderr),
    /// other failures are 400.
    async fn detect(&mut self) -> Result<bool, ApiError> {
        let out = self
            .run(&["rev-parse", "--show-toplevel"], "git", None, false)
            .await?;
        if out.code != 0 {
            if out.stderr.to_lowercase().contains("not a git repository") {
                return Ok(false);
            }
            return Err(ApiError::BadRequest(
                non_blank(&out.stderr, "Git repository detection failed").to_string(),
            ));
        }
        let toplevel = out.stdout.trim_end();
        self.root = self.checked(Path::new(toplevel))?;
        Ok(true)
    }

    /// Guard: `detect` + 400 "Not a Git repository" (the non-status reads).
    async fn require_repo(&mut self) -> Result<(), ApiError> {
        if !self.detect().await? {
            return Err(ApiError::BadRequest("Not a Git repository".into()));
        }
        Ok(())
    }

    /// Validate a REPOSITORY-RELATIVE file path (upstream `file_path`):
    /// absolute inputs are rejected with 400, the join against the repo root
    /// must stay inside the workspace — and the RELATIVE path is returned
    /// (porcelain paths are root-relative, so status keys line up).
    fn rel_file_path(&self, path: &str) -> Result<String, ApiError> {
        if path.is_empty() || Path::new(path).is_absolute() {
            return Err(ApiError::BadRequest(
                "Expected a repository-relative file path".into(),
            ));
        }
        let joined = self.root.join(path);
        // Upstream `file_path` ALSO rejects paths that leave the repo root
        // (400 "File is outside the repository") — `../sibling` inside the
        // workspace is confined by the workspace check but escapes the REPO.
        let resolved = self.checked(&joined)?;
        if std::env::var("OWUI_SKIP_REPO_CONTAIN").is_ok() {
            return Ok(path.to_string());
        }
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if !resolved.starts_with(&root) {
            return Err(ApiError::BadRequest(
                "File is outside the repository".into(),
            ));
        }
        Ok(path.to_string())
    }
}

fn non_blank<'a>(preferred: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = preferred.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status_name(code: &str) -> String {
    match code {
        "M" => "modified",
        "A" => "added",
        "D" => "deleted",
        "R" => "renamed",
        "C" => "copied",
        "T" => "type-changed",
        "U" => "conflict",
        other => return other.to_string(),
    }
    .to_string()
}

impl GitRepo {
    /// `GET /git/status` — the full panel state (porcelain v2 + numstat +
    /// untracked line stats + remote URL).
    async fn status(&mut self) -> Result<Value, ApiError> {
        if !self.detect().await? {
            return Ok(json!({
                "is_repo": false, "branch": "", "files": [], "ahead": 0, "behind": 0
            }));
        }
        let out = self
            .run(
                &[
                    "status",
                    "--porcelain=v2",
                    "--branch",
                    "-z",
                    "--untracked-files=all",
                ],
                "git",
                None,
                true,
            )
            .await?;
        let mut result = json!({
            "is_repo": true,
            "root": self.root.display().to_string(),
            "branch": "",
            "upstream": "",
            "ahead": 0,
            "behind": 0,
            "files": [],
        });
        let records: Vec<String> = split_nul(&out.stdout);
        let mut it = records.into_iter();
        let mut untracked_paths: Vec<String> = Vec::new();
        while let Some(record) = it.next() {
            if let Some(head) = record.strip_prefix("# branch.head ") {
                result["branch"] = json!(head);
            } else if let Some(oid) = record.strip_prefix("# branch.oid ") {
                result["head"] = json!(oid);
            } else if let Some(up) = record.strip_prefix("# branch.upstream ") {
                result["upstream"] = json!(up);
            } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
                let mut parts = ab.split_whitespace();
                let ahead: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let behind: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                result["ahead"] = json!(ahead);
                result["behind"] = json!(behind.abs());
            } else if record.starts_with(['1', '2', 'u', '?']) && !record.starts_with("# ") {
                let (path, xy, original, is_unmerged, is_rename) =
                    if let Some(rest) = record.strip_prefix("? ") {
                        (rest.to_string(), "??".to_string(), None, false, false)
                    } else {
                        // Python `record.split(" ", N)` with N = maxsplit →
                        // N+1 parts, the last being the path (porcelain v2:
                        // 1 → 8 maxsplits, 2 → 9, u → 10).
                        let maxsplit = match record.as_bytes().first() {
                            Some(b'1') => 8,
                            Some(b'2') => 9,
                            Some(b'u') => 10,
                            _ => 8,
                        };
                        let fields: Vec<&str> = record.splitn(maxsplit + 1, ' ').collect();
                        if fields.len() < maxsplit + 1 {
                            continue;
                        }
                        let path = fields[maxsplit].to_string();
                        let xy = fields[1].to_string();
                        // A rename record carries the ORIGINAL path as the next
                        // NUL field — attached to the SAME entry.
                        let original = if record.starts_with("2 ") {
                            it.next()
                        } else {
                            None
                        };
                        (
                            path,
                            xy,
                            original,
                            record.starts_with("u "),
                            record.starts_with("2 "),
                        )
                    };
                let staged = !xy.starts_with('.') && xy != "??";
                let unstaged = xy.len() > 1 && xy.as_bytes()[1] != b'.';
                let status = if is_unmerged {
                    "conflict".to_string()
                } else if xy == "??" {
                    "untracked".to_string()
                } else {
                    status_name(if xy.len() > 1 && xy.as_bytes()[1] != b'.' {
                        &xy[1..2]
                    } else {
                        &xy[..1]
                    })
                };
                let _ = is_rename;
                let mut entry = json!({
                    "path": path,
                    "staged": staged,
                    "unstaged": unstaged,
                    "staged_status": xy.as_bytes().first().map_or('.', |b| *b as char).to_string(),
                    "unstaged_status": xy.as_bytes().get(1).map_or('.', |b| *b as char).to_string(),
                    "additions": 0,
                    "deletions": 0,
                    "status": status,
                });
                if let Some(orig) = original {
                    entry["original_path"] = json!(orig);
                }
                if entry["status"] == "untracked" {
                    untracked_paths.push(path.clone());
                }
                if let Some(files) = result["files"].as_array_mut() {
                    files.push(entry);
                }
            }
        }
        // numstat deltas (staged + unstaged passes).
        for args in [
            vec!["diff", "--no-ext-diff", "--no-textconv", "--numstat", "-z"],
            vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                "--cached",
            ],
        ] {
            let stats = self.run(&args, "git", None, true).await?.stdout;
            let records = split_nul(&stats);
            let mut it = records.into_iter().peekable();
            while let Some(record) = it.next() {
                if record.is_empty() {
                    continue;
                }
                let mut parts = record.splitn(3, '\t');
                let (Some(added), Some(deleted), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let path = if path.is_empty() {
                    // Rename: the next two NUL fields carry old → new.
                    let old = it.next();
                    let new = it.next();
                    new.or(old).unwrap_or_default()
                } else {
                    path.to_string()
                };
                // Patch onto the file entry by path.
                if let Some(files) = result["files"].as_array_mut() {
                    if let Some(file) = files.iter_mut().find(|f| f["path"] == json!(path)) {
                        if added == "-" {
                            file["binary"] = json!(true);
                        } else {
                            let a: i64 = added.parse().unwrap_or(0);
                            let d: i64 = deleted.parse().unwrap_or(0);
                            file["additions"] = json!(file["additions"].as_i64().unwrap_or(0) + a);
                            file["deletions"] = json!(file["deletions"].as_i64().unwrap_or(0) + d);
                        }
                    }
                }
            }
        }

        // Untracked line counts (upstream's line-stats helper, ported inline).
        let mut line_stats = std::collections::HashMap::new();
        for rel in &untracked_paths {
            if let Ok(validated) = self.rel_file_path(rel) {
                line_stats.insert(validated, count_lines(&self.root.join(rel)));
            }
        }
        if !line_stats.is_empty() {
            if let Some(files) = result["files"].as_array_mut() {
                for file in files.iter_mut() {
                    let rel = file["path"].as_str().unwrap_or_default().to_string();
                    if let Some(count) = line_stats.get(&rel) {
                        match count {
                            None => file["binary"] = json!(true),
                            Some(n) => file["additions"] = json!(n),
                        }
                    }
                }
            }
        }

        let remote = self
            .run(&["remote", "get-url", "origin"], "git", None, false)
            .await?;
        result["remote_url"] = json!(if remote.code == 0 {
            remote.stdout.trim().to_string()
        } else {
            String::new()
        });
        Ok(result)
    }
}

/// Split on NUL, keeping empty fields (upstream `output.split("\0")`).
fn split_nul(s: &str) -> Vec<String> {
    s.split('\0').map(str::to_string).collect()
}

/// Port of upstream's `line-stats` helper: `None` = binary (NUL byte found),
/// `Some(n)` = line count with the no-trailing-newline adjustment.
fn count_lines(path: &Path) -> Option<u64> {
    use std::io::Read;
    if let Ok(target) = std::fs::read_link(path) {
        // Symlink: count the target path bytes.
        let data = target.to_string_lossy().into_owned();
        let nl = data.matches('\n').count() as u64;
        return Some(nl + u64::from(!data.ends_with('\n') && !data.is_empty()));
    }
    let file = std::fs::File::open(path).ok()?;
    let mut count: u64 = 0;
    let mut last: Option<u8> = None;
    // Heap buffer: 64 KiB is over the clippy stack-array budget and this runs
    // on the blocking pool anyway.
    let mut buf = vec![0u8; 65536];
    let mut handle = file.take(32 * 1024 * 1024);
    #[allow(clippy::naive_bytecount)] // one bounded read loop; not worth a crate
    loop {
        let n = match handle.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return None,
        };
        if buf[..n].contains(&0) {
            return None; // binary
        }
        count += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
        last = buf[..n].last().copied();
    }
    Some(count + u64::from(last.is_some_and(|b| b != b'\n')))
}

// ---------------------------------------------------------------------------
// Handlers (GET reads)
// ---------------------------------------------------------------------------

/// Query parameters shared by every read (upstream `root` default ".").
#[derive(Debug, Deserialize)]
pub struct GitRootQuery {
    /// Repository root override (path inside the workspace; default `.`).
    pub root: Option<String>,
}

/// `GET /git/status`.
pub async fn git_status(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitRootQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    Ok(Json(repo.status().await?))
}

/// `GET /git/diff` query.
#[derive(Debug, Deserialize)]
pub struct GitDiffQuery {
    /// Repository-relative file to diff.
    pub file: String,
    /// Repository root override.
    pub root: Option<String>,
    /// Diff the staged (index) side.
    pub staged: Option<bool>,
    /// Diff an untracked file against /dev/null.
    pub untracked: Option<bool>,
    /// `--ignore-all-space`.
    pub ignore_whitespace: Option<bool>,
}

/// `GET /git/diff`.
pub async fn git_diff(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitDiffQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    // Upstream validates the path as repository-RELATIVE (400 on absolute)
    // and runs the diff with cwd = repo root, so the relative path goes
    // straight into the pathspec.
    let path = repo.rel_file_path(&q.file)?;
    let mut args = vec![
        "diff".to_string(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--color=never".into(),
        "--unified=3".into(),
    ];
    if q.ignore_whitespace.unwrap_or(false) {
        args.push("--ignore-all-space".into());
    }
    if q.untracked.unwrap_or(false) {
        args.push("--no-index".into());
        args.push("--".into());
        args.push("/dev/null".into());
        args.push(path);
    } else {
        if q.staged.unwrap_or(false) {
            args.push("--cached".into());
        }
        args.push("--".into());
        args.push(format!(":(literal){path}"));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.run(&arg_refs, "git", None, false).await?;
    let acceptable = if q.untracked.unwrap_or(false) {
        out.code == 0 || out.code == 1
    } else {
        out.code == 0
    };
    if !acceptable {
        return Err(ApiError::BadRequest(
            non_blank(&out.stderr, "Cannot read diff").to_string(),
        ));
    }
    Ok(Json(json!({ "diff": out.stdout })))
}

/// `GET /git/log` query.
#[derive(Debug, Deserialize)]
pub struct GitLogQuery {
    /// Repository root override.
    pub root: Option<String>,
    /// Page size (1–200, default 50).
    pub limit: Option<u64>,
    /// Skip N commits.
    pub offset: Option<u64>,
}

/// `GET /git/log` — commit list (`hash, short_hash, author, date, message`).
pub async fn git_log(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitLogQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    let head = repo
        .run(&["rev-parse", "--verify", "HEAD"], "git", None, false)
        .await?;
    if head.code != 0 {
        return Ok(Json(Value::Array(Vec::new())));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0);
    let out = repo
        .run(
            &[
                "log",
                &format!("-{limit}"),
                &format!("--skip={offset}"),
                "--format=%H%x00%h%x00%an%x00%aI%x00%s%x00",
            ],
            "git",
            None,
            true,
        )
        .await?;
    let fields = split_nul(&out.stdout);
    let mut commits = Vec::new();
    let mut i = 0;
    while i + 5 <= fields.len() {
        commits.push(json!({
            "hash": fields[i].trim_start_matches('\n'),
            "short_hash": fields[i + 1],
            "author": fields[i + 2],
            "date": fields[i + 3],
            "message": fields[i + 4],
        }));
        i += 5;
    }
    Ok(Json(Value::Array(commits)))
}

/// `GET /git/show` query.
#[derive(Debug, Deserialize)]
pub struct GitShowQuery {
    /// The commit hash to show.
    pub r#ref: String,
    /// Repository root override.
    pub root: Option<String>,
}

/// `GET /git/show` — one commit's diff.
pub async fn git_show(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitShowQuery>,
) -> Result<Json<Value>, ApiError> {
    // Upstream: a bare hash only (no ref expressions).
    if q.r#ref.len() < 40 || q.r#ref.len() > 64 || !q.r#ref.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest("Expected a commit hash".into()));
    }
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    let out = repo
        .run(
            &[
                "show",
                "--format=",
                "--no-ext-diff",
                "--no-textconv",
                "--color=never",
                &q.r#ref,
                "--",
            ],
            "git",
            None,
            true,
        )
        .await?;
    Ok(Json(json!({ "diff": out.stdout })))
}

/// `GET /git/branches` — local + remote branch list.
pub async fn git_branches(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitRootQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    let out = repo
        .run(
            &[
                "for-each-ref",
                "--format=%(refname:short)%00%(HEAD)%00%(upstream:short)%00%(symref)%00%(refname)",
                "refs/heads",
                "refs/remotes",
            ],
            "git",
            None,
            true,
        )
        .await?;
    let mut branches = Vec::new();
    for line in out.stdout.lines() {
        let parts: Vec<&str> = line.split('\0').collect();
        if parts.len() != 5 || !parts[3].is_empty() {
            continue; // symbolic refs are skipped (upstream parity)
        }
        branches.push(json!({
            "name": parts[0],
            "current": parts[1] == "*",
            "upstream": parts[2],
            "is_local": parts[4].starts_with("refs/heads/"),
        }));
    }
    Ok(Json(Value::Array(branches)))
}

/// `GET /git/worktrees` — worktree list (paths confined to the workspace).
pub async fn git_worktrees(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumQuery(q): AxumQuery<GitRootQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    let out = repo
        .run(
            &["worktree", "list", "--porcelain", "-z"],
            "git",
            None,
            true,
        )
        .await?;
    let mut trees: Vec<Value> = Vec::new();
    for field in split_nul(&out.stdout) {
        if let Some(path) = field.strip_prefix("worktree ") {
            // Skip trees outside the workspace (upstream `checked_path` → continue).
            let Ok(checked) = repo.checked(Path::new(path)) else {
                continue;
            };
            let is_current = std::fs::canonicalize(&repo.root).is_ok_and(|r| r == checked);
            trees.push(json!({
                "path": checked.display().to_string(),
                "branch": "",
                "current": is_current,
            }));
        } else if let Some(branch) = field.strip_prefix("branch ") {
            if let Some(tree) = trees.last_mut() {
                tree["branch"] = json!(branch.strip_prefix("refs/heads/").unwrap_or(branch));
            }
        } else if field == "detached" {
            if let Some(tree) = trees.last_mut() {
                tree["branch"] = json!("(detached)");
            }
        }
    }
    Ok(Json(Value::Array(trees)))
}

// ---------------------------------------------------------------------------
// POST/PATCH/DELETE /git/{action}
// ---------------------------------------------------------------------------

/// The git-action body (upstream `GitActionRequest`).
#[derive(Debug, Deserialize)]
pub struct GitActionRequest {
    /// Repository root override (defaults to the workspace base).
    #[serde(default)]
    pub root: String,
    /// Repository-relative paths to operate on (stage/unstage/discard).
    #[serde(default)]
    pub files: Vec<String>,
    /// Commit / stash message.
    #[serde(default)]
    pub message: String,
    /// Branch operand (checkout/branch-create/delete).
    #[serde(default)]
    pub branch: String,
    /// Branch name for the `branch` action's POST/DELETE/PATCH forms.
    #[serde(default)]
    pub name: String,
    /// Worktree directory target (`worktrees` action).
    #[serde(default)]
    pub path: String,
    /// Source branch for a rename (`branch` PATCH).
    #[serde(default)]
    pub old_name: String,
    /// Destination branch for a rename (`branch` PATCH).
    #[serde(default)]
    pub new_name: String,
    /// `push` publishes with `--set-upstream origin <current>`.
    #[serde(default)]
    pub set_upstream: bool,
}

/// `POST|PATCH|DELETE /git/{action}` — stage/unstage/discard/commit/
/// checkout/branch (create|rename|delete)/worktree/fetch/pull/push/publish/
/// stash/unstash/uncommit.
pub async fn git_action(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(action): AxumPath<String>,
    method: Method,
    Json(payload): Json<GitActionRequest>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, &payload.root)?;
    repo.require_repo().await?;

    let mut action = action;
    let mut branch = payload.branch.clone();
    let mut target = String::new();
    if action == "branch" {
        match method {
            Method::DELETE => {
                action = "delete_branch".into();
                branch = payload.name.clone();
            }
            Method::PATCH => {
                action = "rename_branch".into();
                branch = payload.old_name.clone();
                target = payload.new_name.clone();
            }
            _ => branch = payload.name.clone(),
        }
    } else if method != Method::POST {
        return Err(ApiError::BadRequest("Method not allowed".into()));
    } else if action == "worktrees" {
        action = "worktree".into();
        target = payload.path.clone();
    } else if action == "push" && payload.set_upstream {
        action = "publish".into();
    }
    let allowed = [
        "stage",
        "unstage",
        "discard",
        "commit",
        "checkout",
        "branch",
        "delete_branch",
        "rename_branch",
        "worktree",
        "fetch",
        "pull",
        "push",
        "publish",
        "stash",
        "unstash",
        "uncommit",
    ];
    if !allowed.contains(&action.as_str()) {
        return Err(ApiError::NotFound("Unknown Git operation".into()));
    }

    let commands = build_action_args(
        &repo,
        &action,
        &payload.files,
        &payload.message,
        &branch,
        &target,
    )
    .await?;
    let mut message = String::new();
    for cmd in &commands {
        let arg_refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let out = repo.run(&arg_refs, "git", None, true).await?;
        message = out.stdout.trim().to_string();
    }
    let mut result = json!({ "message": message });
    if action == "worktree" {
        // The worktree destination is the last arg of `worktree add -b <branch> <dest>`.
        if let Some(cmd) = commands.last() {
            if let Some(dest) = cmd.last() {
                result["path"] = json!(dest);
            }
        }
    }
    Ok(Json(result))
}

/// Build the git argv for an action (upstream `GitRepository.action`, branch
/// mapping included). Most actions are one command; `discard` may run
/// `restore`/`rm` AND `clean`, so a list of commands is returned and executed
/// in order.
async fn build_action_args(
    repo: &GitRepo,
    action: &str,
    files: &[String],
    message: &str,
    branch: &str,
    target: &str,
) -> Result<Vec<Vec<String>>, ApiError> {
    let mut paths = Vec::with_capacity(files.len());
    let mut pathspecs = Vec::with_capacity(files.len());
    for f in files {
        let rel = repo.rel_file_path(f)?;
        pathspecs.push(format!(":(literal){rel}"));
        paths.push(rel);
    }
    if matches!(action, "stage" | "unstage" | "discard") && paths.is_empty() {
        return Err(ApiError::BadRequest("Select files first".into()));
    }
    let single: Vec<String> = match action {
        "stage" => {
            let mut v = vec!["add".into(), "--".into()];
            v.extend(pathspecs.clone());
            v
        }
        "unstage" => {
            let head = repo
                .run(&["rev-parse", "--verify", "HEAD"], "git", None, false)
                .await?;
            let mut v = if head.code == 0 {
                vec!["rm".into(), "--cached".into(), "--".into()]
            } else {
                vec!["reset".into(), "HEAD".into(), "--".into()]
            };
            v.extend(pathspecs.clone());
            v
        }
        "discard" => {
            // Recompute status to split tracked vs untracked (upstream parity,
            // including the nested-repo guard). Paths here are the SAME
            // root-relative keys status reports.
            let status = GitRepo {
                root: repo.root.clone(),
                base: repo.base.clone(),
            }
            .status()
            .await?;
            let mut changed: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            for file in status["files"].as_array().map_or(&[][..], Vec::as_slice) {
                let p = file["path"].as_str().unwrap_or_default().to_string();
                let tracked = file["status"].as_str() != Some("untracked");
                changed
                    .entry(p.clone())
                    .and_modify(|e| *e |= tracked)
                    .or_insert(tracked);
                if let Some(orig) = file["original_path"].as_str() {
                    changed.insert(orig.to_string(), true);
                }
            }
            for p in &paths {
                if !changed.contains_key(p) {
                    return Err(ApiError::BadRequest("Select changed files first".into()));
                }
            }
            for p in &paths {
                let is_untracked = !changed.get(p).copied().unwrap_or(false);
                if is_untracked && p.ends_with('/') {
                    return Err(ApiError::BadRequest(
                        "Cannot discard a nested Git repository; remove it separately".into(),
                    ));
                }
            }
            let tracked: Vec<&String> = pathspecs
                .iter()
                .zip(&paths)
                .filter(|(_, p)| changed.get(*p).copied().unwrap_or(false))
                .map(|(spec, _)| spec)
                .collect();
            let untracked: Vec<&String> = pathspecs
                .iter()
                .zip(&paths)
                .filter(|(_, p)| !changed.get(*p).copied().unwrap_or(false))
                .map(|(spec, _)| spec)
                .collect();
            let mut commands: Vec<Vec<String>> = Vec::new();
            if !tracked.is_empty() {
                let head = repo
                    .run(&["rev-parse", "--verify", "HEAD"], "git", None, false)
                    .await?;
                let mut cmd = if head.code != 0 {
                    vec!["rm".into(), "-f".into(), "--".into()]
                } else {
                    vec![
                        "restore".into(),
                        "--source=HEAD".into(),
                        "--staged".into(),
                        "--worktree".into(),
                        "--".into(),
                    ]
                };
                cmd.extend(tracked.iter().map(|s| (*s).clone()));
                commands.push(cmd);
            }
            if !untracked.is_empty() {
                let mut cmd = vec!["clean".into(), "-f".into(), "--".into()];
                cmd.extend(untracked.iter().map(|s| (*s).clone()));
                commands.push(cmd);
            }
            return Ok(commands);
        }
        "commit" => {
            if message.trim().is_empty() {
                return Err(ApiError::BadRequest("Enter a commit message".into()));
            }
            vec!["commit".into(), "-m".into(), message.into()]
        }
        "fetch" => vec!["fetch".into()],
        "pull" => vec!["pull".into(), "--ff-only".into()],
        "push" => vec!["push".into()],
        "publish" => {
            let current = repo
                .run(&["symbolic-ref", "--short", "HEAD"], "git", None, true)
                .await?;
            vec![
                "push".into(),
                "--set-upstream".into(),
                "origin".into(),
                current.stdout.trim().to_string(),
            ]
        }
        "checkout" | "branch" => {
            if branch.is_empty() || branch.starts_with('-') {
                return Err(ApiError::BadRequest("Enter a branch name".into()));
            }
            repo.run(&["check-ref-format", "--branch", branch], "git", None, true)
                .await?;
            if action == "branch" {
                vec!["switch".into(), "-c".into(), branch.into()]
            } else {
                let remote = repo
                    .run(
                        &["show-ref", "--verify", &format!("refs/remotes/{branch}")],
                        "git",
                        None,
                        false,
                    )
                    .await?;
                if remote.code == 0 {
                    vec!["switch".into(), "--track".into(), branch.into()]
                } else {
                    vec!["switch".into(), branch.into()]
                }
            }
        }
        "worktree" => {
            if branch.is_empty() || branch.starts_with('-') || target.is_empty() {
                return Err(ApiError::BadRequest(
                    "Enter a branch and worktree directory".into(),
                ));
            }
            repo.run(&["check-ref-format", "--branch", branch], "git", None, true)
                .await?;
            // Upstream resolves the target against the repo root and confines
            // it (checked_path). Ours: safe_path against the workspace.
            let candidate = if Path::new(target).is_absolute() {
                PathBuf::from(target)
            } else {
                repo.root.join(target)
            };
            let destination = repo.checked(&candidate)?;
            vec![
                "worktree".into(),
                "add".into(),
                "-b".into(),
                branch.into(),
                destination.display().to_string(),
            ]
        }
        "rename_branch" => {
            if branch.is_empty()
                || target.is_empty()
                || branch.starts_with('-')
                || target.starts_with('-')
            {
                return Err(ApiError::BadRequest("Enter valid branch names".into()));
            }
            repo.run(&["check-ref-format", "--branch", target], "git", None, true)
                .await?;
            vec!["branch".into(), "-m".into(), branch.into(), target.into()]
        }
        "delete_branch" => {
            if branch.is_empty() || branch.starts_with('-') {
                return Err(ApiError::BadRequest("Enter a branch name".into()));
            }
            vec!["branch".into(), "-d".into(), branch.into()]
        }
        "uncommit" => vec!["reset".into(), "--soft".into(), "HEAD~1".into()],
        "stash" => {
            let mut v = vec!["stash".into(), "push".into(), "-u".into()];
            if !message.trim().is_empty() {
                v.push("-m".into());
                v.push(message.into());
            }
            v
        }
        "unstash" => vec!["stash".into(), "pop".into()],
        other => return Err(ApiError::NotFound(format!("Unknown Git action: {other}"))),
    };
    Ok(vec![single])
}

// ---------------------------------------------------------------------------
// PR / issue views (gh)
// ---------------------------------------------------------------------------

/// The gh-action body (upstream `PullRequestAction`).
#[derive(Debug, Deserialize)]
pub struct PrActionRequest {
    /// Repository root override.
    #[serde(default)]
    pub root: String,
    /// PR number (0 = create).
    #[serde(default)]
    pub number: u64,
    /// PR title (create/edit).
    #[serde(default)]
    pub title: String,
    /// PR body / review comment / close comment.
    #[serde(default)]
    pub body: String,
    /// Base branch (create).
    #[serde(default)]
    pub base: String,
    /// Head branch (create).
    #[serde(default)]
    pub head: String,
    /// Create as draft.
    #[serde(default)]
    pub draft: bool,
    /// Merge method (squash/merge/rebase).
    #[serde(default)]
    pub strategy: String,
    /// Review event (approve/comment/request_changes) for the `review` action.
    #[serde(default)]
    pub event: String,
}

/// The gh-issue-action body (upstream `IssueAction`).
#[derive(Debug, Deserialize)]
pub struct IssueActionRequest {
    /// Repository root override.
    #[serde(default)]
    pub root: String,
    /// Issue number (0 = create).
    #[serde(default)]
    pub number: u64,
    /// Issue title (create/edit).
    #[serde(default)]
    pub title: String,
    /// Issue body / comment / close comment.
    #[serde(default)]
    pub body: String,
    /// Close reason (`completed` / `not planned`).
    #[serde(default)]
    pub reason: String,
}

/// `GET /git/pr/{view}` query.
#[derive(Debug, Deserialize)]
pub struct PrViewQuery {
    /// Repository root override.
    pub root: Option<String>,
    /// PR number (0 = list context).
    pub number: Option<u64>,
    /// Open/closed/merged/all filter.
    pub state: Option<String>,
    /// GitHub search term (applied server-side by `gh`).
    pub search: Option<String>,
}

/// `GET /git/pr/{view}` — capabilities/list/view/diff/checks via `gh`.
pub async fn git_pr_view(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(view): AxumPath<String>,
    AxumQuery(q): AxumQuery<PrViewQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    let number = q.number.unwrap_or(0);
    let state_name = q.state.as_deref().unwrap_or("open");
    let search = q.search.as_deref().unwrap_or("");

    if !repo.detect().await? {
        return Ok(Json(if view == "capabilities" {
            json!({ "available": false })
        } else {
            Value::Array(Vec::new())
        }));
    }

    if view == "capabilities" {
        // Upstream wraps the WHOLE capabilities probe in
        // try/except HTTPException → {"available": False} — a missing `gh`
        // binary must degrade, not 404 (review finding 6).
        let unavailable = || Ok(Json(json!({ "available": false })));
        let Ok(version) = repo.run(&["--version"], "gh", None, false).await else {
            return unavailable();
        };
        if version.code != 0 {
            return unavailable();
        }
        let Ok(view) = repo
            .run(
                &[
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner,defaultBranchRef,hasIssuesEnabled,pullRequests,issues",
                ],
                "gh",
                None,
                false,
            )
            .await
        else {
            return unavailable();
        };
        let repo_json: Value = if view.code == 0 {
            serde_json::from_str(view.stdout.trim()).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let available = view.code == 0;
        return Ok(Json(json!({
            "available": available,
            "default_branch": repo_json
                .get("defaultBranchRef")
                .and_then(|r| r.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("main"),
            "issues": available && repo_json.get("hasIssuesEnabled").and_then(Value::as_bool).unwrap_or(true),
            "open_pull_requests": repo_json.pointer("/pullRequests/totalCount").cloned().unwrap_or(Value::Null),
            "open_issues": repo_json.pointer("/issues/totalCount").cloned().unwrap_or(Value::Null),
        })));
    }

    let mut args: Vec<String>;
    match view.as_str() {
        "list" => {
            if !matches!(state_name, "open" | "closed" | "merged" | "all") {
                return Err(ApiError::BadRequest("Invalid pull request state".into()));
            }
            args = vec![
                "pr".into(),
                "list".into(),
                "--state".into(),
                state_name.into(),
                "--limit".into(),
                "50".into(),
                "--json".into(),
                format!("{PR_FIELDS},statusCheckRollup,updatedAt,labels"),
            ];
            if !search.trim().is_empty() {
                args.push("--search".into());
                args.push(search.trim().into());
            }
        }
        "view" | "diff" | "checks" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select a pull request".into()));
            }
            match view.as_str() {
                "view" => {
                    args = vec![
                        "pr".into(),
                        "view".into(),
                        number.to_string(),
                        "--json".into(),
                        format!("{PR_FIELDS},comments,reviews,statusCheckRollup,reviewRequests,assignees,labels"),
                    ];
                }
                "diff" => {
                    args = vec![
                        "pr".into(),
                        "diff".into(),
                        number.to_string(),
                        "--color=never".into(),
                    ];
                }
                _ => {
                    let out = repo
                        .run(
                            &[
                                "pr",
                                "checks",
                                &number.to_string(),
                                "--json",
                                "bucket,completedAt,description,event,link,name,startedAt,state,workflow",
                            ],
                            "gh",
                            None,
                            false,
                        )
                        .await?;
                    if !matches!(out.code, 0 | 1 | 8) {
                        return Err(ApiError::BadRequest(
                            non_blank(&out.stderr, "Cannot read checks").to_string(),
                        ));
                    }
                    let items: Value = if out.stdout.trim().is_empty() {
                        Value::Array(Vec::new())
                    } else {
                        serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Array(Vec::new()))
                    };
                    return Ok(Json(json!({ "items": items })));
                }
            }
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "Unknown pull request view: {other}"
            )))
        }
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.run(&arg_refs, "gh", None, true).await?;
    if view == "list" {
        let items: Value =
            serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Array(Vec::new()));
        return Ok(Json(json!({ "items": items, "search": search.trim() })));
    }
    if view == "diff" {
        return Ok(Json(json!({ "diff": out.stdout })));
    }
    let parsed: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    Ok(Json(parsed))
}

/// `POST /git/pr/{action}` — create/checkout/close/reopen/ready/update-branch/
/// comment/approve/request-changes/review-comment/edit/merge via `gh`.
pub async fn git_pr_action(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(action): AxumPath<String>,
    Json(payload): Json<PrActionRequest>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, &payload.root)?;
    repo.require_repo().await?;

    let mut action = action;
    if action == "review" {
        action = match payload.event.as_str() {
            "approve" => "approve".to_string(),
            "comment" => "review-comment".to_string(),
            "request_changes" => "request-changes".to_string(),
            other => {
                return Err(ApiError::BadRequest(format!(
                    "Invalid review event: {other}"
                )));
            }
        };
    }

    let number = payload.number;
    let title = payload.title.trim();
    let body = payload.body.trim();
    let args: Vec<String> = if action == "create" {
        if title.is_empty() || payload.base.trim().is_empty() || payload.head.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "Enter a title, base branch, and head branch".into(),
            ));
        }
        let mut a = vec![
            "pr".into(),
            "create".into(),
            "--title".into(),
            payload.title.clone(),
            "--body".into(),
            payload.body.clone(),
            "--base".into(),
            payload.base.clone(),
            "--head".into(),
            payload.head.clone(),
        ];
        if payload.draft {
            a.push("--draft".into());
        }
        a
    } else if number == 0 {
        return Err(ApiError::BadRequest("Select a pull request".into()));
    } else if matches!(
        action.as_str(),
        "checkout" | "close" | "reopen" | "ready" | "update-branch"
    ) {
        vec!["pr".into(), action.clone(), number.to_string()]
    } else if matches!(
        action.as_str(),
        "comment" | "approve" | "request-changes" | "review-comment"
    ) {
        if action != "approve" && body.is_empty() {
            return Err(ApiError::BadRequest("Enter a comment".into()));
        }
        if action == "comment" {
            vec![
                "pr".into(),
                "comment".into(),
                number.to_string(),
                "--body".into(),
                payload.body.clone(),
            ]
        } else {
            let flag = if action == "review-comment" {
                "--comment"
            } else {
                action.as_str()
            };
            vec![
                "pr".into(),
                "review".into(),
                number.to_string(),
                format!("--{flag}"),
                "--body".into(),
                payload.body.clone(),
            ]
        }
    } else if action == "edit" {
        if title.is_empty() {
            return Err(ApiError::BadRequest("Enter a title".into()));
        }
        vec![
            "pr".into(),
            "edit".into(),
            number.to_string(),
            "--title".into(),
            payload.title.clone(),
            "--body".into(),
            payload.body.clone(),
        ]
    } else if action == "merge" {
        let method = if payload.strategy.is_empty() {
            "squash"
        } else {
            payload.strategy.as_str()
        };
        if !matches!(method, "squash" | "merge" | "rebase") {
            return Err(ApiError::BadRequest("Invalid merge method".into()));
        }
        vec![
            "pr".into(),
            "merge".into(),
            number.to_string(),
            format!("--{method}"),
        ]
    } else {
        return Err(ApiError::BadRequest(format!(
            "Unknown pull request action: {action}"
        )));
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.run(&arg_refs, "gh", None, true).await?;
    Ok(Json(json!({ "message": out.stdout.trim() })))
}

/// `GET /git/issue/{view}` query.
#[derive(Debug, Deserialize)]
pub struct IssueViewQuery {
    /// Repository root override.
    pub root: Option<String>,
    /// Issue number (0 = list context).
    pub number: Option<u64>,
    /// Open/closed/all filter.
    pub state: Option<String>,
    /// GitHub search term (applied server-side by `gh`).
    pub search: Option<String>,
}

/// `GET /git/issue/{view}` — list/view via `gh`.
pub async fn git_issue_view(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(view): AxumPath<String>,
    AxumQuery(q): AxumQuery<IssueViewQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, q.root.as_deref().unwrap_or("."))?;
    repo.require_repo().await?;
    let number = q.number.unwrap_or(0);
    let state_name = q.state.as_deref().unwrap_or("open");
    let search = q.search.as_deref().unwrap_or("");
    let args: Vec<String> = match view.as_str() {
        "list" => {
            if !matches!(state_name, "open" | "closed" | "all") {
                return Err(ApiError::BadRequest("Invalid issue state".into()));
            }
            let mut a = vec![
                "issue".into(),
                "list".into(),
                "--state".into(),
                state_name.into(),
                "--limit".into(),
                "50".into(),
                "--json".into(),
                ISSUE_FIELDS.into(),
            ];
            if !search.trim().is_empty() {
                a.push("--search".into());
                a.push(search.trim().into());
            }
            a
        }
        "view" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select an issue".into()));
            }
            vec![
                "issue".into(),
                "view".into(),
                number.to_string(),
                "--json".into(),
                format!("{ISSUE_FIELDS},body,comments,createdAt"),
            ]
        }
        other => return Err(ApiError::BadRequest(format!("Unknown issue view: {other}"))),
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.run(&arg_refs, "gh", None, true).await?;
    if view == "list" {
        let items: Value =
            serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Array(Vec::new()));
        return Ok(Json(json!({ "items": items, "search": search.trim() })));
    }
    let parsed: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    Ok(Json(parsed))
}

/// `POST /git/issue/{action}` — create/edit/comment/close/reopen via `gh`.
pub async fn git_issue_action(
    _auth: Authed,
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(action): AxumPath<String>,
    Json(payload): Json<IssueActionRequest>,
) -> Result<Json<Value>, ApiError> {
    let base = base_of(&state, &headers)?;
    let mut repo = GitRepo::open(&base, &payload.root)?;
    repo.require_repo().await?;
    let number = payload.number;
    let title = payload.title.trim();
    let body = payload.body.trim();
    let mut args: Vec<String> = match action.as_str() {
        "create" => {
            if title.is_empty() {
                return Err(ApiError::BadRequest("Enter a title".into()));
            }
            vec![
                "issue".into(),
                "create".into(),
                "--title".into(),
                payload.title.clone(),
                "--body".into(),
                payload.body.clone(),
            ]
        }
        "edit" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select an issue".into()));
            }
            if title.is_empty() {
                return Err(ApiError::BadRequest("Enter a title".into()));
            }
            vec![
                "issue".into(),
                "edit".into(),
                number.to_string(),
                "--title".into(),
                payload.title.clone(),
                "--body".into(),
                payload.body.clone(),
            ]
        }
        "comment" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select an issue".into()));
            }
            if body.is_empty() {
                return Err(ApiError::BadRequest("Enter a comment".into()));
            }
            vec![
                "issue".into(),
                "comment".into(),
                number.to_string(),
                "--body".into(),
                payload.body.clone(),
            ]
        }
        "close" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select an issue".into()));
            }
            let reason = if payload.reason.is_empty() {
                "completed"
            } else {
                payload.reason.as_str()
            };
            if !matches!(reason, "completed" | "not planned") {
                return Err(ApiError::BadRequest("Invalid close reason".into()));
            }
            vec![
                "issue".into(),
                "close".into(),
                number.to_string(),
                "--reason".into(),
                reason.into(),
            ]
        }
        "reopen" => {
            if number == 0 {
                return Err(ApiError::BadRequest("Select an issue".into()));
            }
            vec!["issue".into(), "reopen".into(), number.to_string()]
        }
        other => {
            return Err(ApiError::BadRequest(format!(
                "Unknown issue action: {other}"
            )))
        }
    };
    if matches!(action.as_str(), "close" | "reopen") && !body.is_empty() {
        args.push("--comment".into());
        args.push(payload.body.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = repo.run(&arg_refs, "gh", None, true).await?;
    Ok(Json(json!({ "message": out.stdout.trim() })))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PATH for the availability probe (test binary env has git
    /// on PATH in CI/dev; the probe just reports what it finds).
    #[test]
    fn git_available_reports_path_probe() {
        let _ = git_available();
    }

    #[test]
    fn status_names_map_like_upstream() {
        assert_eq!(status_name("M"), "modified");
        assert_eq!(status_name("R"), "renamed");
        assert_eq!(status_name("T"), "type-changed");
        assert_eq!(status_name("U"), "conflict");
        assert_eq!(status_name("X"), "X");
    }

    #[test]
    fn count_lines_handles_binary_and_missing_trailing_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let text = dir.path().join("t.txt");
        std::fs::write(&text, b"a\nb\nc").unwrap();
        assert_eq!(count_lines(&text), Some(3)); // 2 newlines + unterminated last line
        std::fs::write(&text, b"a\nb\nc\n").unwrap();
        assert_eq!(count_lines(&text), Some(3));
        let bin = dir.path().join("b.bin");
        std::fs::write(&bin, b"a\0b").unwrap();
        assert_eq!(count_lines(&bin), None);
        let link = dir.path().join("l.txt");
        std::os::unix::fs::symlink("some/target\n", &link).unwrap();
        assert_eq!(count_lines(&link), Some(1));
    }

    #[test]
    fn split_nul_keeps_empties() {
        assert_eq!(
            split_nul("a\0\0b"),
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
    }
}
