//! The durable record of checkpoints: which exist, what hash each must still
//! have, and which files Cairn wrote under each.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ChkError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Git,
    Fs,
}

/// One checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub session: String,
    pub turn: u64,
    /// Monotonic across the whole store (not per session).
    pub seq: u64,
    pub label: String,
    pub created_at: String,
    pub backend: Backend,
    /// Git only: the private ref, the commit on it, and the tree hash the
    /// ref must still resolve to.
    pub ref_name: Option<String>,
    pub commit_oid: Option<String>,
    pub tree_oid: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// Workspace-relative path → hash of what Cairn last wrote there, under
    /// this checkpoint. A later user edit makes the file's hash differ.
    #[serde(default)]
    pub touched: BTreeMap<String, String>,
    /// Copy backend: paths that did not exist before Cairn created them.
    #[serde(default)]
    pub created: Vec<String>,
    /// Copy backend: paths whose pre-image is saved.
    #[serde(default)]
    pub saved: Vec<String>,
    /// A snapshot taken by undo so redo has somewhere to return to.
    #[serde(default)]
    pub redo_point: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub next_seq: u64,
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    /// Per session: checkpoint ids undone, most recent last (redo stack).
    #[serde(default)]
    pub redo: BTreeMap<String, Vec<String>>,
    /// Per session: checkpoint ids restored from, most recent last.
    #[serde(default)]
    pub undone: BTreeMap<String, Vec<String>>,
}

/// Where the index lives.
#[derive(Debug, Clone)]
pub struct MetaStore {
    path: PathBuf,
}

impl MetaStore {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("index.json"),
        }
    }

    /// Read the index; a missing file is an empty one.
    ///
    /// # Errors
    /// `E-CHK-FAIL` when the file exists and is not an index.
    pub fn load(&self) -> Result<Index, ChkError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                ChkError::fail(format!(
                    "{} is not a checkpoint index: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(ChkError::fail(format!(
                "cannot read {}: {e}",
                self.path.display()
            ))),
        }
    }

    /// Write the index through a temp file and a rename.
    ///
    /// # Errors
    /// `E-CHK-FAIL` when it cannot be written.
    pub fn save(&self, index: &Index) -> Result<(), ChkError> {
        let fail = |e: &dyn std::fmt::Display| {
            ChkError::fail(format!("cannot write {}: {e}", self.path.display()))
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fail(&e))?;
        }
        let text = serde_json::to_string_pretty(index).map_err(|e| fail(&e))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| fail(&e))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            fail(&e)
        })
    }
}

/// A path recorded in metadata, validated: relative, no `..`, no drive or
/// root. Metadata sits in a file the workspace owner can edit, so its paths
/// are untrusted until proven otherwise.
#[must_use]
pub fn safe_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') || rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    if rel.chars().nth(1) == Some(':') {
        return false;
    }
    !rel.split(['/', '\\'])
        .any(|part| part == ".." || part.is_empty())
}
