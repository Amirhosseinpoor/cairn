//! `cairn-git` — checkpoints and undo (SPEC §9.8).
//!
//! A checkpoint is a snapshot of the working tree taken before a turn, so the
//! turn can be undone. Two mechanisms, one interface:
//!
//! * **In a git repository** the snapshot is a commit on a private ref
//!   (`refs/cairn/checkpoints/<session>/<seq>`) built from a *temporary copy*
//!   of the index, so the user's index, stash and branches are never touched
//!   (REQ-SAFE-016) and untracked files are captured too.
//! * **Elsewhere** the pre-image of each file is copied aside just before
//!   Cairn first writes it.
//!
//! Undo defaults to *Cairn's files only* ([`RestorePolicy::CairnFilesOnly`]):
//! only paths Cairn wrote are restored, and only if the user has not edited
//! them since. Anything else is a conflict (`E-CHK-MERGE`) that the caller
//! may escalate to [`RestorePolicy::Full`] with an explicit confirmation.
//!
//! Metadata about every checkpoint (including the tree hash it must still
//! have, and which files Cairn wrote under it) lives in a JSON file *outside*
//! the ref, so a ref that has been moved is detected rather than trusted
//! (`E-CHK-HASH`).

mod checkpointer;
mod fs_backend;
mod git_backend;
mod meta;
pub mod ops;

use std::path::Path;

pub use checkpointer::{Checkpointer, Limits, Restored, Target};
pub use git_backend::{Change, Status};
pub use meta::{Backend, Checkpoint};

use cairn_core::error::codes;

/// Told about every write Cairn makes, so a checkpoint can protect the file
/// first (the copy backend) and remember what Cairn left behind (both).
pub trait WriteObserver: Send + Sync {
    /// About to replace or create `abs`; called before anything is written.
    fn before_write(&self, abs: &Path);
    /// `abs` now holds content hashing to `sha256`.
    fn after_write(&self, abs: &Path, sha256: &str);
    /// A named snapshot is wanted now (before a commit, §9.8).
    fn checkpoint(&self, _label: &str) {}
}

/// A [`WriteObserver`] that records nothing, for runs with checkpoints off.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl WriteObserver for NoObserver {
    fn before_write(&self, _abs: &Path) {}
    fn after_write(&self, _abs: &Path, _sha256: &str) {}
}

/// What went wrong, with the stable code and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ChkError {
    pub code: &'static str,
    pub message: String,
    pub recovery: Option<String>,
    /// Paths involved, for a conflict.
    pub paths: Vec<String>,
}

impl ChkError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: None,
            paths: Vec::new(),
        }
    }

    pub(crate) fn recovery(mut self, recovery: impl Into<String>) -> Self {
        self.recovery = Some(recovery.into());
        self
    }

    pub(crate) fn fail(message: impl Into<String>) -> Self {
        Self::new(codes::CHK_FAIL, message)
    }
}

/// How much undo may touch (§9.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePolicy {
    /// Only files Cairn wrote, and only if the user has not edited them since.
    CairnFilesOnly,
    /// Everything the checkpoint captured. Overwrites the user's own changes;
    /// callers must have asked.
    Full,
}

impl RestorePolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CairnFilesOnly => "cairn_files_only",
            Self::Full => "full",
        }
    }
}
