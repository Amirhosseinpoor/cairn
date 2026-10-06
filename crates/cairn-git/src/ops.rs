//! Reading a repository for the `git_status` and `git_diff` tools, and
//! making commits for `git_commit` (SPEC §6.2.12–§6.2.14).
//!
//! Status and diff use libgit2. Committing runs the `git` program instead:
//! it is the only way to honour hooks, signing and the user's configuration
//! exactly as `git commit` would.

use std::path::{Path, PathBuf};
use std::process::Command;

use cairn_core::error::codes;
use git2::{
    BranchType, Delta, DiffFormat, DiffOptions, ErrorCode, Repository, RepositoryState, Status,
    StatusOptions,
};
use serde::Serialize;

/// A failure with its stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct GitError {
    pub code: &'static str,
    pub message: String,
    pub recovery: Option<String>,
}

impl GitError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: None,
        }
    }

    fn recovery(mut self, text: impl Into<String>) -> Self {
        self.recovery = Some(text.into());
        self
    }
}

fn cmd_error(e: &git2::Error) -> GitError {
    GitError::new(codes::GIT_CMD, format!("git: {}", e.message()))
}

/// Open the repository around `dir`.
///
/// # Errors
/// `E-GIT-NOREPO` when there is none.
pub fn open(dir: &Path) -> Result<Repository, GitError> {
    Repository::discover(dir).map_err(|e| {
        if e.code() == ErrorCode::NotFound {
            GitError::new(
                codes::GIT_NOREPO,
                "this directory is not inside a git repository.",
            )
            .recovery("Run `git init`, or work in a directory that is tracked by git.")
        } else {
            cmd_error(&e)
        }
    })
}

fn workdir(repo: &Repository) -> Result<PathBuf, GitError> {
    repo.workdir()
        .map(Path::to_path_buf)
        .ok_or_else(|| GitError::new(codes::GIT_NOREPO, "a bare repository has no working tree."))
}

/// A path as a pathspec relative to the working directory, or `None` for "all".
fn pathspec(repo: &Repository, path: &Path) -> Result<Option<String>, GitError> {
    let root = workdir(repo)?;
    let root = root.canonicalize().unwrap_or(root);
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let rel = abs
        .strip_prefix(&root)
        .map_err(|_| GitError::new(codes::GIT_NOREPO, "that path is outside the repository."))?;
    let text = rel.to_string_lossy().replace('\\', "/");
    Ok((!text.is_empty()).then_some(text))
}

// ----------------------------------------------------------------- status

/// §6.2.12's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
    pub conflicts: Vec<String>,
    pub clean: bool,
    pub detached: bool,
    pub rebase_in_progress: bool,
}

/// Most entries listed per list; the model asks again with a narrower path.
const LIST_CAP: usize = 500;

fn tag(prefix: &str, path: &str) -> String {
    format!("{prefix} {path}")
}

/// The state of the working tree under `path`.
///
/// # Errors
/// `E-GIT-NOREPO` or `E-GIT-CMD`.
pub fn status(path: &Path) -> Result<Report, GitError> {
    let repo = open(path)?;
    let spec = pathspec(&repo, path)?;
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .include_ignored(false);
    if let Some(spec) = &spec {
        options.pathspec(spec);
    }
    let statuses = repo
        .statuses(Some(&mut options))
        .map_err(|e| cmd_error(&e))?;
    let (mut staged, mut unstaged, mut untracked, mut conflicts) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for entry in statuses.iter() {
        let Some(name) = entry.path() else { continue };
        let s = entry.status();
        if s.contains(Status::CONFLICTED) {
            conflicts.push(name.to_string());
            continue;
        }
        if s.contains(Status::WT_NEW) {
            untracked.push(name.to_string());
            continue;
        }
        let index = if s.contains(Status::INDEX_NEW) {
            Some("A")
        } else if s.contains(Status::INDEX_MODIFIED) || s.contains(Status::INDEX_TYPECHANGE) {
            Some("M")
        } else if s.contains(Status::INDEX_DELETED) {
            Some("D")
        } else if s.contains(Status::INDEX_RENAMED) {
            Some("R")
        } else {
            None
        };
        if let Some(code) = index {
            staged.push(tag(code, name));
        }
        let work = if s.contains(Status::WT_MODIFIED) || s.contains(Status::WT_TYPECHANGE) {
            Some("M")
        } else if s.contains(Status::WT_DELETED) {
            Some("D")
        } else if s.contains(Status::WT_RENAMED) {
            Some("R")
        } else {
            None
        };
        if let Some(code) = work {
            unstaged.push(tag(code, name));
        }
    }
    for list in [&mut staged, &mut unstaged, &mut untracked, &mut conflicts] {
        list.truncate(LIST_CAP);
    }
    let (branch, upstream, ahead, behind, detached) = branch_info(&repo);
    let rebase_in_progress = matches!(
        repo.state(),
        RepositoryState::Rebase | RepositoryState::RebaseInteractive | RepositoryState::RebaseMerge
    );
    let clean =
        staged.is_empty() && unstaged.is_empty() && untracked.is_empty() && conflicts.is_empty();
    Ok(Report {
        branch,
        upstream,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
        conflicts,
        clean,
        detached,
        rebase_in_progress,
    })
}

fn branch_info(repo: &Repository) -> (Option<String>, Option<String>, usize, usize, bool) {
    let detached = repo.head_detached().unwrap_or(false);
    let Ok(head) = repo.head() else {
        // An unborn branch still has a name.
        let name = repo
            .find_reference("HEAD")
            .ok()
            .and_then(|r| r.symbolic_target().map(str::to_string))
            .and_then(|t| t.strip_prefix("refs/heads/").map(str::to_string));
        return (name, None, 0, 0, false);
    };
    if detached {
        return (None, None, 0, 0, true);
    }
    let Some(name) = head.shorthand().map(str::to_string) else {
        return (None, None, 0, 0, false);
    };
    let mut upstream = None;
    let (mut ahead, mut behind) = (0, 0);
    if let Ok(branch) = repo.find_branch(&name, BranchType::Local) {
        if let Ok(up) = branch.upstream() {
            upstream = up.name().ok().flatten().map(str::to_string);
            if let (Some(local), Some(remote)) = (head.target(), up.get().target()) {
                if let Ok((a, b)) = repo.graph_ahead_behind(local, remote) {
                    ahead = a;
                    behind = b;
                }
            }
        }
    }
    (Some(name), upstream, ahead, behind, false)
}

// ------------------------------------------------------------------- diff

/// What a diff covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Index to working tree.
    Working,
    /// HEAD to index.
    Staged,
    /// HEAD to working tree.
    All,
    /// A commit against its first parent.
    Commit,
}

/// §6.2.13's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffReport {
    pub diff: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub truncated: bool,
}

/// Cut `text` to at most `max` bytes at a line boundary.
fn fit(text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').map_or(end, |i| i + 1);
    (text[..cut].to_string(), true)
}

/// The diff for `scope` under `path`.
///
/// # Errors
/// `E-GIT-NOREPO`, `E-GIT-BADREV` (unknown commit, or `scope: commit`
/// without one), `E-GIT-NODIFF` (nothing differs).
pub fn diff(
    path: &Path,
    scope: Scope,
    commit: Option<&str>,
    context: u32,
    max_bytes: usize,
) -> Result<DiffReport, GitError> {
    let repo = open(path)?;
    let spec = pathspec(&repo, path)?;
    let mut options = DiffOptions::new();
    options
        .context_lines(context)
        .include_untracked(matches!(scope, Scope::Working | Scope::All))
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    if let Some(spec) = &spec {
        options.pathspec(spec);
    }
    let head_tree = || repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let diff = match scope {
        Scope::Working => repo.diff_index_to_workdir(None, Some(&mut options)),
        Scope::Staged => repo.diff_tree_to_index(head_tree().as_ref(), None, Some(&mut options)),
        Scope::All => {
            repo.diff_tree_to_workdir_with_index(head_tree().as_ref(), Some(&mut options))
        }
        Scope::Commit => {
            let rev = commit.ok_or_else(|| {
                GitError::new(codes::GIT_BADREV, "scope `commit` needs a `commit`.")
                    .recovery("Pass a sha, tag or branch name as `commit`.")
            })?;
            let object = repo.revparse_single(rev).map_err(|_| {
                GitError::new(
                    codes::GIT_BADREV,
                    format!("`{rev}` is not a commit in this repository."),
                )
                .recovery("Use `git_status` or a bash `git log` to find a valid revision.")
            })?;
            let commit = object.peel_to_commit().map_err(|_| {
                GitError::new(
                    codes::GIT_BADREV,
                    format!("`{rev}` does not name a commit."),
                )
            })?;
            let new = commit.tree().map_err(|e| cmd_error(&e))?;
            let old = commit.parent(0).ok().and_then(|p| p.tree().ok());
            repo.diff_tree_to_tree(old.as_ref(), Some(&new), Some(&mut options))
        }
    }
    .map_err(|e| cmd_error(&e))?;

    let stats = diff.stats().map_err(|e| cmd_error(&e))?;
    if diff.deltas().all(|d| d.status() == Delta::Unmodified) {
        return Err(
            GitError::new(codes::GIT_NODIFF, "there are no differences.")
                .recovery("The tree matches what was compared; nothing to show."),
        );
    }
    let mut out = Vec::new();
    diff.print(DiffFormat::Patch, |_, _, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            out.push(line.origin() as u8);
        }
        out.extend_from_slice(line.content());
        true
    })
    .map_err(|e| cmd_error(&e))?;
    let (text, truncated) = fit(String::from_utf8_lossy(&out).into_owned(), max_bytes);
    Ok(DiffReport {
        diff: text,
        files_changed: stats.files_changed(),
        insertions: stats.insertions(),
        deletions: stats.deletions(),
        truncated,
    })
}

// ----------------------------------------------------------------- commit

/// What `git_commit` asks for.
#[derive(Debug, Clone, Default)]
pub struct CommitRequest {
    pub message: String,
    /// Stage every tracked modification first.
    pub all: bool,
    /// Paths to stage first, relative to the working directory.
    pub paths: Vec<String>,
    pub amend: bool,
    pub allow_empty: bool,
}

/// §6.2.14's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitReport {
    pub sha: String,
    pub short_sha: String,
    pub message: String,
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

fn git_cli(dir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| {
            GitError::new(codes::GIT_CMD, format!("could not run `git`: {e}"))
                .recovery("Install git and make sure it is on PATH.")
        })
}

/// Paths (relative to the working directory) that are modified or deleted
/// among tracked files, from the status.
///
/// # Errors
/// `E-GIT-NOREPO` or `E-GIT-CMD`.
pub fn tracked_changes(dir: &Path) -> Result<Vec<String>, GitError> {
    let repo = open(dir)?;
    let mut options = StatusOptions::new();
    options.include_untracked(false);
    let statuses = repo
        .statuses(Some(&mut options))
        .map_err(|e| cmd_error(&e))?;
    let mut out = Vec::new();
    for entry in statuses.iter() {
        let s = entry.status();
        let touched = s.intersects(
            Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE | Status::WT_RENAMED,
        );
        if let (true, Some(path)) = (touched, entry.path()) {
            out.push(path.to_string());
        }
    }
    Ok(out)
}

/// Paths in the index that differ from HEAD.
///
/// # Errors
/// `E-GIT-NOREPO` or `E-GIT-CMD`.
pub fn staged_paths(dir: &Path) -> Result<Vec<String>, GitError> {
    let repo = open(dir)?;
    let mut options = StatusOptions::new();
    options.include_untracked(false);
    let statuses = repo
        .statuses(Some(&mut options))
        .map_err(|e| cmd_error(&e))?;
    Ok(statuses
        .iter()
        .filter(|e| {
            e.status().intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED
                    | Status::INDEX_TYPECHANGE,
            )
        })
        .filter_map(|e| e.path().map(str::to_string))
        .collect())
}

/// The abbreviated sha of HEAD, if there is one.
#[must_use]
pub fn head_short(dir: &Path) -> Option<String> {
    let repo = open(dir).ok()?;
    let id = repo.head().ok()?.target()?;
    Some(id.to_string().chars().take(7).collect())
}

/// Whether the user's identity is configured.
fn identity_set(repo: &Repository) -> bool {
    repo.signature().is_ok()
}

/// Stage `paths` (additions, changes and removals) with `git add -A --`.
///
/// # Errors
/// `E-GIT-LOCK`, `E-GIT-CMD`.
pub fn stage(dir: &Path, paths: &[String]) -> Result<(), GitError> {
    for chunk in paths.chunks(100) {
        let mut args = vec!["add", "-A", "--"];
        args.extend(chunk.iter().map(String::as_str));
        let out = git_cli(dir, &args)?;
        if !out.status.success() {
            return Err(classify(&String::from_utf8_lossy(&out.stderr), "git add"));
        }
    }
    Ok(())
}

fn classify(stderr: &str, what: &str) -> GitError {
    if stderr.contains("index.lock") {
        GitError::new(
            codes::GIT_LOCK,
            ".git/index.lock present — another git process is running; retry in 2s.",
        )
        .recovery("Wait a moment and try again.")
    } else if stderr.contains("unmerged") || stderr.contains("Unmerged") {
        GitError::new(codes::GIT_CONFLICT, "there are unmerged paths.")
            .recovery("Resolve the conflicts and stage the result first.")
    } else {
        GitError::new(codes::GIT_CMD, format!("{what} failed: {}", stderr.trim()))
    }
}

/// Make the commit. Staging of `paths`/`all` is the caller's job (it has the
/// ignore rules); this checks the repository, runs `git commit` and reports.
///
/// # Errors
/// `E-GIT-NOREPO`, `E-GIT-NOCFG`, `E-GIT-EMPTY`, `E-GIT-CONFLICT`,
/// `E-GIT-LOCK`, `E-GIT-PRECOMMIT`, `E-GIT-CMD`.
pub fn commit(dir: &Path, request: &CommitRequest) -> Result<CommitReport, GitError> {
    let repo = open(dir)?;
    if repo.index().map_err(|e| cmd_error(&e))?.has_conflicts() {
        return Err(
            GitError::new(codes::GIT_CONFLICT, "there are unmerged paths.")
                .recovery("Resolve the conflicts and stage the result first."),
        );
    }
    if !identity_set(&repo) {
        return Err(GitError::new(
            codes::GIT_NOCFG,
            "git user.name and user.email are not set.",
        )
        .recovery("Ask the user to run `git config user.name` and `git config user.email`."));
    }
    let nothing_staged = staged_paths(dir)?.is_empty();
    if nothing_staged && !request.allow_empty && !request.amend {
        return Err(
            GitError::new(codes::GIT_EMPTY, "Nothing staged. Pass all:true or paths.").recovery(
                "Stage changes with `paths`, or pass `all: true` to include tracked modifications.",
            ),
        );
    }
    let mut args = vec!["commit", "-q", "-m", request.message.as_str()];
    if request.amend {
        args.push("--amend");
    }
    if request.allow_empty {
        args.push("--allow-empty");
    }
    let out = git_cli(dir, &args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let all = format!("{stdout}{stderr}");
        if stderr.contains("index.lock") || stderr.contains("unmerged") {
            return Err(classify(&stderr, "git commit"));
        }
        if all.contains("nothing to commit") || all.contains("no changes added") {
            return Err(
                GitError::new(codes::GIT_EMPTY, "Nothing staged. Pass all:true or paths.")
                    .recovery("Stage changes with `paths`, or pass `all: true`."),
            );
        }
        // Anything else that stops a commit with a normal message is a hook.
        let hook = repo.path().join("hooks").join("pre-commit").exists()
            || repo.path().join("hooks").join("commit-msg").exists();
        if hook {
            return Err(GitError::new(
                codes::GIT_PRECOMMIT,
                format!("a git hook rejected the commit:\n{}", all.trim()),
            )
            .recovery("Fix the issue reported by the hook, then commit again."));
        }
        return Err(GitError::new(
            codes::GIT_CMD,
            format!("git commit failed: {}", all.trim()),
        ));
    }
    let repo = open(dir)?;
    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|e| cmd_error(&e))?;
    let new = head.tree().map_err(|e| cmd_error(&e))?;
    let old = head.parent(0).ok().and_then(|p| p.tree().ok());
    let stats = repo
        .diff_tree_to_tree(old.as_ref(), Some(&new), None)
        .and_then(|d| d.stats())
        .map_err(|e| cmd_error(&e))?;
    let sha = head.id().to_string();
    Ok(CommitReport {
        short_sha: sha.chars().take(7).collect(),
        sha,
        message: head.summary().unwrap_or_default().to_string(),
        files: stats.files_changed(),
        insertions: stats.insertions(),
        deletions: stats.deletions(),
    })
}
