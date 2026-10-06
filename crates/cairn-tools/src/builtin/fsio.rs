//! What the write tools share: loading a file for editing, the stale-file
//! rules of §6.3.3, syntax validation under its time budget (§6.3.6), and
//! atomic writes that keep a file's permissions.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cairn_core::error::codes;
use cairn_search::{classify, Content, SNIFF_BYTES};
use sha2::{Digest, Sha256};

use super::common::io_error;
use crate::edit::Document;
use crate::paths::Resolved;
use crate::types::{Access, SyntaxProblem, SyntaxVerdict, ToolContext, ToolError};

/// §5.1: files above this are not edited.
pub const MAX_EDIT_BYTES: u64 = 8 * 1024 * 1024;
/// REQ-TOOL-015.
pub const SYNTAX_BUDGET: Duration = Duration::from_millis(500);

/// A file as the write tools see it.
#[derive(Debug)]
pub struct Loaded {
    pub doc: Document,
    /// Hash of the raw bytes on disk.
    pub sha256: String,
    pub mode: Option<u32>,
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Resolve `input` for writing, re-validated at execution time
/// (REQ-SAFE-007).
pub fn resolve_for_write(ctx: &ToolContext, input: &str) -> Result<Resolved, ToolError> {
    let resolved = ctx.boundary.resolve(input, &ctx.cwd, Access::Write)?;
    if resolved.protected {
        return Err(ToolError::new(
            codes::FS_PROTECTED,
            format!("`{}` is a protected path.", resolved.display()),
        ));
    }
    Ok(resolved)
}

/// Read and decode an existing file, or `None` when it is not there.
pub fn load(path: &Path, shown: &str) -> Result<Option<Loaded>, ToolError> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(shown, &e)),
    };
    if meta.is_dir() {
        return Err(
            ToolError::new(codes::FS_DIR, format!("`{shown}` is a directory."))
                .recovery("Path is a directory; choose a file path."),
        );
    }
    if meta.len() > MAX_EDIT_BYTES {
        return Err(ToolError::new(
            codes::FS_TOOBIG,
            format!(
                "`{shown}` is {} bytes; files over {MAX_EDIT_BYTES} are not edited.",
                meta.len()
            ),
        )
        .recovery("Edit it with bash, or work on a smaller file."));
    }
    let bytes = std::fs::read(path).map_err(|e| io_error(shown, &e))?;
    match classify(&bytes[..bytes.len().min(SNIFF_BYTES)], meta.len()) {
        Content::Text { .. } => {}
        Content::Binary => {
            return Err(
                ToolError::new(codes::FS_BINARY, format!("`{shown}` is a binary file."))
                    .recovery("Binary files cannot be edited."),
            )
        }
        Content::Utf16 { .. } | Content::NotUtf8 => {
            return Err(
                ToolError::new(codes::FS_ENCODING, format!("`{shown}` is not UTF-8 text."))
                    .recovery("Only UTF-8 files can be edited; convert it first."),
            )
        }
    }
    let doc = Document::decode(&bytes).map_err(|e| {
        ToolError::new(
            codes::FS_ENCODING,
            format!("`{shown}` is not valid UTF-8 (byte {}).", e.valid_up_to),
        )
        .recovery("Only UTF-8 files can be edited; convert it first.")
    })?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    Ok(Some(Loaded {
        doc,
        sha256: sha256_hex(&bytes),
        mode,
    }))
}

/// §6.3.3's verdict on a file the model may be working from an old copy of.
#[derive(Debug, PartialEq, Eq)]
pub enum Staleness {
    /// Nothing known, or what is known is current.
    Fresh,
    /// The model's copy (`seen`) is not what is on disk now.
    Changed { seen: String },
}

/// Compare the current hash against what the caller says (`expected`) or the
/// session last observed.
#[must_use]
pub fn staleness(
    ctx: &ToolContext,
    path: &Path,
    current: &str,
    expected: Option<&str>,
) -> Staleness {
    let seen = expected
        .map(str::to_string)
        .or_else(|| ctx.file_state.last_seen(path));
    match seen {
        Some(seen) if seen != current => Staleness::Changed { seen },
        _ => Staleness::Fresh,
    }
}

/// The `E-EDIT-STALE` error, with the first differing lines when the old
/// text is on hand (REQ-TOOL-013).
#[must_use]
pub fn stale_edit_error(
    ctx: &ToolContext,
    path: &Path,
    seen: &str,
    current_text: &str,
    current_sha: &str,
) -> ToolError {
    let diff = ctx
        .file_state
        .last_content(path)
        .map(|old| crate::edit::brief_diff(&old, current_text, 20));
    let short = |s: &str| s.chars().take(4).collect::<String>();
    let mut error = ToolError::new(
        codes::EDIT_STALE,
        format!(
            "The file changed since you read it (sha {}→{}).{}",
            short(seen),
            short(current_sha),
            diff.as_deref()
                .map_or(String::new(), |d| format!("\nWhat changed:\n{d}")),
        ),
    )
    .recovery("The file changed since you read it. Re-read it, then retry.");
    if let Some(diff) = diff {
        error = error.with_data(serde_json::json!({ "diff": diff }));
    }
    error
}

/// Run the syntax check under its budget. A check that overruns does not
/// block the write (REQ-TOOL-015).
pub async fn check_syntax(
    ctx: &ToolContext,
    shown: &str,
    before: &str,
    after: &str,
) -> SyntaxVerdict {
    let syntax = std::sync::Arc::clone(&ctx.syntax);
    let (shown, before, after) = (shown.to_string(), before.to_string(), after.to_string());
    let job = tokio::task::spawn_blocking(move || syntax.check(&shown, &before, &after));
    match tokio::time::timeout(SYNTAX_BUDGET, job).await {
        Ok(Ok(verdict)) => verdict,
        // A panicking validator is a defect in the validator, not a reason to
        // refuse the edit.
        Ok(Err(_)) => SyntaxVerdict::Unchecked,
        Err(_) => SyntaxVerdict::TimedOut,
    }
}

/// The error for a rejected edit (§6.3.6 step 4). The file is untouched.
#[must_use]
pub fn syntax_error(shown: &str, problem: &SyntaxProblem) -> ToolError {
    ToolError::new(
        codes::EDIT_SYNTAX,
        format!(
            "The edit would leave `{shown}` with a syntax error at line {}, column {}.{}{}\n{}",
            problem.line,
            problem.column,
            problem
                .expected
                .as_deref()
                .map_or(String::new(), |e| format!(" Expected {e}.")),
            problem
                .found
                .as_deref()
                .map_or(String::new(), |f| format!(" Found {f}.")),
            problem.snippet,
        ),
    )
    .recovery("Fix the syntax issue shown; the file was left unchanged.")
    .with_data(serde_json::json!({
        "line": problem.line,
        "column": problem.column,
        "expected": problem.expected,
        "found": problem.found,
        "snippet": problem.snippet,
    }))
}

/// `syntax_ok` for an output: `Some(true)`, or `None` (JSON null) when no
/// check applied or it timed out.
#[must_use]
pub fn syntax_ok(verdict: &SyntaxVerdict) -> Option<bool> {
    match verdict {
        SyntaxVerdict::Valid => Some(true),
        _ => None,
    }
}

/// Write `bytes` to `path` through a sibling temp file and a rename, keeping
/// `mode` on a file that already existed. A crash leaves the old content or
/// the new, never a mixture.
pub fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    create_dirs: bool,
) -> Result<(), ToolError> {
    let fail = |e: &std::io::Error| {
        let kind = e.kind();
        let (code, why) = match kind {
            std::io::ErrorKind::PermissionDenied => (codes::FS_PERM, "permission denied"),
            std::io::ErrorKind::NotFound => {
                (codes::FS_NOPARENT, "the parent directory does not exist")
            }
            _ => (codes::FS_READONLY, "the filesystem refused the write"),
        };
        ToolError::new(
            code,
            format!("cannot write `{}`: {why} ({e})", path.display()),
        )
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        if !create_dirs {
            return Err(ToolError::new(
                codes::FS_NOPARENT,
                format!("the directory `{}` does not exist.", parent.display()),
            )
            .recovery("Pass create_dirs:true, or create the directory first."));
        }
        std::fs::create_dir_all(parent).map_err(|e| fail(&e))?;
    }
    let tmp: PathBuf = parent.join(format!(
        ".cairn-write-{}-{}.tmp",
        std::process::id(),
        path.file_name()
            .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned())
    ));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode & 0o7777))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(&e));
    }
    Ok(())
}

/// `(added, removed)` line counts between two texts.
#[must_use]
pub fn line_delta(old: &str, new: &str) -> (usize, usize) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .fold((0, 0), |(a, r), c| match c.tag() {
            ChangeTag::Insert => (a + 1, r),
            ChangeTag::Delete => (a, r + 1),
            ChangeTag::Equal => (a, r),
        })
}
