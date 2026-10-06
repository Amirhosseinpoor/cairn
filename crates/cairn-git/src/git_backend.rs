//! The git half: a snapshot commit on a private ref, built from a temporary
//! copy of the index so the user's index is never opened for writing.

use std::path::{Path, PathBuf};

use git2::{Delta, DiffOptions, IndexAddOption, Oid, Repository, Signature};

use crate::ChkError;

fn g(e: &git2::Error) -> ChkError {
    ChkError::fail(format!("git: {}", e.message()))
}

/// What a snapshot produced.
pub struct Snapshot {
    pub ref_name: String,
    pub commit: Oid,
    pub tree: Oid,
    pub files: u64,
}

/// A change between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

pub struct Git {
    /// The `.git` directory; handles are opened per call because packs
    /// written by another handle are not seen by a long-lived one.
    gitdir: PathBuf,
    /// The working directory (relative paths are relative to it).
    pub workdir: PathBuf,
    /// Scratch space for the temporary index.
    scratch: PathBuf,
}

impl std::fmt::Debug for Git {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Git")
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl Git {
    /// Open the repository containing `workspace`, if there is a usable one.
    pub fn discover(workspace: &Path) -> Option<Self> {
        let repo = Repository::discover(workspace).ok()?;
        let workdir = repo.workdir()?.to_path_buf();
        let gitdir = repo.path().to_path_buf();
        let scratch = gitdir.join("cairn");
        Some(Self {
            gitdir,
            workdir,
            scratch,
        })
    }

    fn open(&self) -> Result<Repository, ChkError> {
        Repository::open(&self.gitdir).map_err(|e| g(&e))
    }

    pub fn store_dir(&self) -> PathBuf {
        self.scratch.clone()
    }

    /// Run `f` on a fresh handle whose new objects collect in memory and are
    /// then written as one pack: a thousand loose objects cost ten times more.
    fn batched<T>(
        &self,
        f: impl FnOnce(&Repository) -> Result<T, ChkError>,
    ) -> Result<T, ChkError> {
        let repo = self.open()?;
        let odb = repo.odb().map_err(|e| g(&e))?;
        let pack = odb.add_new_mempack_backend(1000).map_err(|e| g(&e))?;
        let value = f(&repo)?;
        let mut buf = git2::Buf::new();
        pack.dump(&repo, &mut buf).map_err(|e| g(&e))?;
        // 32 bytes is a pack with no objects.
        if buf.len() > 32 {
            let mut writer = odb.packwriter().map_err(|e| g(&e))?;
            std::io::Write::write_all(&mut writer, &buf)
                .map_err(|e| ChkError::fail(format!("cannot write the pack: {e}")))?;
            writer.commit().map_err(|e| g(&e))?;
        }
        Ok(value)
    }

    /// Build the tree of the working directory as it is now, without writing
    /// to the user's index.
    fn tree_now(&self, repo: &Repository) -> Result<(Oid, u64), ChkError> {
        std::fs::create_dir_all(&self.scratch).map_err(|e| {
            ChkError::fail(format!("cannot create {}: {e}", self.scratch.display()))
        })?;
        let tmp = self
            .scratch
            .join(format!("tmp-index-{}-{}", std::process::id(), nanos()));
        let real = repo.path().join("index");
        if real.exists() {
            std::fs::copy(&real, &tmp)
                .map_err(|e| ChkError::fail(format!("cannot copy the index: {e}")))?;
        }
        let result = (|| {
            let mut index = git2::Index::open(&tmp).map_err(|e| g(&e))?;
            // Binding the copy to the (private) handle makes `add_all` and
            // `write_tree` work on it instead of on `.git/index`.
            repo.set_index(&mut index).map_err(|e| g(&e))?;
            index
                .add_all(["*"], IndexAddOption::DEFAULT, None)
                .map_err(|e| g(&e))?;
            // Tracked files that no longer exist drop out of the snapshot.
            let gone: Vec<String> = index
                .iter()
                .map(|e| String::from_utf8_lossy(&e.path).into_owned())
                .filter(|rel| std::fs::symlink_metadata(self.workdir.join(rel)).is_err())
                .collect();
            for rel in gone {
                let _ = index.remove_path(Path::new(&rel));
            }
            index.remove_all([".cairn"], None).map_err(|e| g(&e))?;
            let files = index.len() as u64;
            let oid = index.write_tree().map_err(|e| g(&e))?;
            Ok((oid, files))
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// Commit the working directory as `ref_name`.
    pub fn snapshot(&self, ref_name: &str, message: &str) -> Result<Snapshot, ChkError> {
        self.batched(|repo| {
            let (tree_oid, files) = self.tree_now(repo)?;
            let tree = repo.find_tree(tree_oid).map_err(|e| g(&e))?;
            let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            let sig = Signature::now("Cairn", "cairn@localhost").map_err(|e| g(&e))?;
            let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
            let commit = repo
                .commit(Some(ref_name), &sig, &sig, message, &tree, &parents)
                .map_err(|e| g(&e))?;
            Ok(Snapshot {
                ref_name: ref_name.to_string(),
                commit,
                tree: tree_oid,
                files,
            })
        })
    }

    /// The tree a ref currently points at, if the ref exists.
    pub fn ref_tree(&self, ref_name: &str) -> Option<Oid> {
        let repo = self.open().ok()?;
        let reference = repo.find_reference(ref_name).ok()?;
        let tree = reference.peel_to_commit().ok()?.tree_id();
        Some(tree)
    }

    pub fn delete_ref(&self, ref_name: &str) {
        if let Ok(repo) = self.open() {
            if let Ok(mut r) = repo.find_reference(ref_name) {
                let _ = r.delete();
            }
        }
    }

    /// Changes from `from` to `to` (`None` = the working directory now).
    pub fn changes(&self, from: Oid, to: Option<Oid>) -> Result<Vec<Change>, ChkError> {
        self.batched(|repo| {
            let to = match to {
                Some(t) => t,
                None => self.tree_now(repo)?.0,
            };
            diff_trees(repo, from, to)
        })
    }

    /// Changes from the working directory now to `to`.
    pub fn changes_from_now(&self, to: Oid) -> Result<Vec<Change>, ChkError> {
        self.batched(|repo| {
            let now = self.tree_now(repo)?.0;
            diff_trees(repo, now, to)
        })
    }

    /// The content of `rel` in `tree`; `None` when the tree lacks it.
    pub fn blob(&self, tree: Oid, rel: &str) -> Option<(Vec<u8>, bool)> {
        let repo = self.open().ok()?;
        let tree = repo.find_tree(tree).ok()?;
        let entry = tree.get_path(Path::new(rel)).ok()?;
        let blob = repo.find_blob(entry.id()).ok()?;
        Some((blob.content().to_vec(), entry.filemode() == 0o100_755))
    }
}

fn diff_trees(repo: &Repository, from: Oid, to: Oid) -> Result<Vec<Change>, ChkError> {
    let a = repo.find_tree(from).map_err(|e| g(&e))?;
    let b = repo.find_tree(to).map_err(|e| g(&e))?;
    let diff = repo
        .diff_tree_to_tree(Some(&a), Some(&b), Some(&mut DiffOptions::new()))
        .map_err(|e| g(&e))?;
    let mut out = Vec::new();
    for delta in diff.deltas() {
        let status = match delta.status() {
            Delta::Added | Delta::Untracked => Status::Added,
            Delta::Deleted => Status::Deleted,
            _ => Status::Modified,
        };
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        if let Some(path) = path {
            out.push(Change { path, status });
        }
    }
    Ok(out)
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}
