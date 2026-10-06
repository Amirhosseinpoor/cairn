//! The checkpoint store: creating, listing, undoing and redoing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use cairn_core::error::codes;
use sha2::{Digest, Sha256};

use crate::fs_backend::{self, Saved};
use crate::git_backend::{Change, Git, Status};
use crate::meta::{safe_rel, Backend, Checkpoint, Index, MetaStore};
use crate::{ChkError, RestorePolicy, WriteObserver};

/// Retention and size limits (§9.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_session: usize,
    pub global: usize,
    pub max_total_bytes: u64,
    /// Copy backend (non-git): the most one turn may save.
    pub fs_max_bytes: u64,
    pub fs_max_files: usize,
    pub undo_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_session: 50,
            global: 200,
            max_total_bytes: 1 << 30,
            fs_max_bytes: 200 << 20,
            fs_max_files: 500,
            undo_depth: 20,
        }
    }
}

/// Which checkpoint an undo names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Last,
    Seq(u64),
    Id(String),
}

/// What an undo or redo did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub checkpoint: String,
    pub policy: RestorePolicy,
    pub files: Vec<String>,
}

#[derive(Default)]
struct State {
    active: Option<String>,
    saved: BTreeSet<String>,
    created: BTreeSet<String>,
    touched: BTreeMap<String, String>,
    bytes: u64,
    disabled: bool,
    warnings: Vec<ChkError>,
}

enum Source {
    Saved(PathBuf),
    Absent,
}

struct Item {
    /// Hash Cairn left (`-` = it left the file absent); `None` = not checked.
    expected: Option<String>,
    source: Source,
}

/// Checkpoints for one session in one workspace.
pub struct Checkpointer {
    root: PathBuf,
    store: PathBuf,
    session: String,
    short: String,
    limits: Limits,
    git: Mutex<Option<Git>>,
    meta: MetaStore,
    state: Mutex<State>,
    /// The turn the last checkpoint belongs to.
    turn: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for Checkpointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpointer")
            .field("root", &self.root)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `path` without a Windows verbatim prefix.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hash_of(abs: &Path) -> String {
    std::fs::read(abs).map_or_else(|_| "-".to_string(), |b| sha(&b))
}

impl Checkpointer {
    /// Open the store for `session` in `workspace`. Never fails: a directory
    /// that is not a usable git repository uses the copy backend.
    #[must_use]
    pub fn open(workspace: &Path, session: &str, limits: Limits) -> Self {
        let given = workspace.to_path_buf();
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        // libgit2 does not understand Windows' `\\?\` verbatim paths.
        let git = Git::discover(&plain(&workspace)).or_else(|| Git::discover(&plain(&given)));
        let (root, store) = match &git {
            Some(g) => (
                g.workdir
                    .canonicalize()
                    .unwrap_or_else(|_| g.workdir.clone()),
                g.store_dir(),
            ),
            None => (
                workspace.clone(),
                workspace.join(".cairn").join("checkpoints"),
            ),
        };
        let mut short: String = session
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        if short.is_empty() {
            short.push('s');
        }
        Self {
            meta: MetaStore::new(&store),
            root,
            store,
            session: session.to_string(),
            short,
            limits,
            git: Mutex::new(git),
            state: Mutex::new(State::default()),
            turn: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Whether snapshots are git commits.
    #[must_use]
    pub fn uses_git(&self) -> bool {
        lock(&self.git).is_some()
    }

    /// Warnings raised since the last call (disk cap, flush failures).
    #[must_use]
    pub fn take_warnings(&self) -> Vec<ChkError> {
        std::mem::take(&mut lock(&self.state).warnings)
    }

    fn rel_of(&self, abs: &Path) -> Option<String> {
        let rel = abs
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                let parent = abs.parent()?.canonicalize().ok()?;
                parent
                    .join(abs.file_name()?)
                    .strip_prefix(&self.root)
                    .ok()
                    .map(Path::to_path_buf)
            })?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        safe_rel(&rel).then_some(rel)
    }

    fn flush(&self, st: &State) -> Result<(), ChkError> {
        let Some(id) = &st.active else {
            return Ok(());
        };
        let mut index = self.meta.load()?;
        if let Some(ck) = index.checkpoints.iter_mut().find(|c| &c.id == id) {
            ck.saved = st.saved.iter().cloned().collect();
            ck.created = st.created.iter().cloned().collect();
            ck.touched = st.touched.clone();
            ck.bytes = st.bytes;
        }
        self.meta.save(&index)
    }

    /// Take the snapshot for a new turn. A failure leaves the turn without a
    /// checkpoint; the caller reports `W-CHK-FAIL` and carries on.
    ///
    /// # Errors
    /// `E-CHK-FAIL` when the snapshot or the index cannot be written.
    pub fn begin_turn(&self, turn: u64, label: &str) -> Result<Checkpoint, ChkError> {
        self.turn.store(turn, std::sync::atomic::Ordering::Relaxed);
        let mut st = lock(&self.state);
        let _ = self.flush(&st);
        *st = State::default();
        let mut index = self.meta.load()?;
        self.drop_redo(&mut index);
        let seq = index.next_seq;
        index.next_seq += 1;
        let id = format!("ck_{}_{seq}", self.short);
        let mut ck = Checkpoint {
            id: id.clone(),
            session: self.session.clone(),
            turn,
            seq,
            label: label.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            backend: Backend::Fs,
            ref_name: None,
            commit_oid: None,
            tree_oid: None,
            files: 0,
            bytes: 0,
            touched: BTreeMap::new(),
            created: Vec::new(),
            saved: Vec::new(),
            redo_point: false,
        };
        if let Some(git) = lock(&self.git).as_ref() {
            let ref_name = format!("refs/cairn/checkpoints/{}/{seq}", self.short);
            let snap = git.snapshot(&ref_name, &format!("cairn checkpoint {label} turn={turn}"))?;
            ck.backend = Backend::Git;
            ck.ref_name = Some(snap.ref_name);
            ck.commit_oid = Some(snap.commit.to_string());
            ck.tree_oid = Some(snap.tree.to_string());
            ck.files = snap.files;
        }
        index.checkpoints.push(ck.clone());
        self.prune(&mut index, &id);
        self.meta.save(&index)?;
        st.active = Some(id);
        Ok(ck)
    }

    /// Persist what the turn wrote.
    pub fn end_turn(&self) {
        let st = lock(&self.state);
        if let Err(e) = self.flush(&st) {
            drop(st);
            lock(&self.state).warnings.push(e);
        }
    }

    /// This session's checkpoints, oldest first.
    ///
    /// # Errors
    /// `E-CHK-FAIL` when the index is unreadable.
    pub fn list(&self) -> Result<Vec<Checkpoint>, ChkError> {
        let mut all: Vec<Checkpoint> = self
            .meta
            .load()?
            .checkpoints
            .into_iter()
            .filter(|c| c.session == self.session && !c.redo_point)
            .collect();
        all.sort_by_key(|c| c.seq);
        Ok(all)
    }

    /// What the turn that began at checkpoint `id` changed: its tree against
    /// the next checkpoint's, or against the working directory for the last.
    ///
    /// # Errors
    /// `E-CHK-FAIL` for an unknown id or an unreadable repository.
    pub fn diff(&self, id: &str) -> Result<Vec<Change>, ChkError> {
        let all = self.list()?;
        let pos = all
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| ChkError::fail(format!("no checkpoint {id}")))?;
        let ck = &all[pos];
        let guard = lock(&self.git);
        let (Some(git), Some(tree)) = (guard.as_ref(), ck.tree_oid.as_deref()) else {
            return Ok(ck
                .touched
                .keys()
                .map(|p| Change {
                    path: p.clone(),
                    status: Status::Modified,
                })
                .collect());
        };
        let from = git2::Oid::from_str(tree).map_err(|e| ChkError::fail(e.to_string()))?;
        let to = match all.get(pos + 1).and_then(|n| n.tree_oid.as_deref()) {
            Some(t) => Some(git2::Oid::from_str(t).map_err(|e| ChkError::fail(e.to_string()))?),
            None => None,
        };
        git.changes(from, to)
    }

    fn remove_ck(&self, ck: &Checkpoint) {
        if let (Some(name), Some(git)) = (&ck.ref_name, lock(&self.git).as_ref()) {
            git.delete_ref(name);
        }
        fs_backend::remove(&self.store, &ck.id);
    }

    fn drop_redo(&self, index: &mut Index) {
        let ids = index.redo.remove(&self.session).unwrap_or_default();
        for id in ids {
            if let Some(pos) = index.checkpoints.iter().position(|c| c.id == id) {
                let ck = index.checkpoints.remove(pos);
                self.remove_ck(&ck);
            }
        }
    }

    fn prune(&self, index: &mut Index, keep: &str) {
        let victim = |index: &Index, pred: &dyn Fn(&Checkpoint) -> bool| {
            index
                .checkpoints
                .iter()
                .filter(|c| c.id != keep && !c.redo_point && pred(c))
                .min_by_key(|c| c.seq)
                .map(|c| c.id.clone())
        };
        loop {
            let session = self.session.clone();
            let mine = index
                .checkpoints
                .iter()
                .filter(|c| c.session == session && !c.redo_point)
                .count();
            let total = index.checkpoints.iter().filter(|c| !c.redo_point).count();
            let bytes: u64 = index.checkpoints.iter().map(|c| c.bytes).sum();
            let id = if mine > self.limits.per_session {
                victim(index, &|c| c.session == session)
            } else if total > self.limits.global || bytes > self.limits.max_total_bytes {
                victim(index, &|_| true)
            } else {
                None
            };
            let Some(id) = id else { return };
            if let Some(pos) = index.checkpoints.iter().position(|c| c.id == id) {
                let ck = index.checkpoints.remove(pos);
                self.remove_ck(&ck);
            }
        }
    }

    /// Restore the files Cairn wrote since `target` to how they were then.
    ///
    /// `hard` is required to go back past a `pre-commit` checkpoint.
    ///
    /// # Errors
    /// `E-CHK-MERGE` when the user edited a file Cairn wrote (nothing is
    /// changed); `E-CHK-HASH` when the checkpoint's ref no longer points at
    /// the tree recorded for it; `E-CHK-FAIL` otherwise.
    pub fn undo(
        &self,
        target: &Target,
        policy: RestorePolicy,
        hard: bool,
    ) -> Result<Restored, ChkError> {
        let _guard = lock(&self.state);
        let mut index = self.meta.load()?;
        let mut mine: Vec<Checkpoint> = index
            .checkpoints
            .iter()
            .filter(|c| c.session == self.session && !c.redo_point)
            .cloned()
            .collect();
        mine.sort_by_key(|c| c.seq);
        let target_ck = match target {
            Target::Last => mine.last(),
            Target::Seq(n) => mine.iter().find(|c| c.seq == *n),
            Target::Id(id) => mine.iter().find(|c| &c.id == id),
        }
        .cloned()
        .ok_or_else(|| {
            ChkError::fail("there is no such checkpoint").recovery("list them with /checkpoints")
        })?;
        let affected: Vec<&Checkpoint> = mine.iter().filter(|c| c.seq >= target_ck.seq).collect();
        if !hard && affected.iter().any(|c| c.label.starts_with("pre-commit")) {
            return Err(
                ChkError::fail("that would undo past a git commit Cairn made")
                    .recovery("repeat with --hard to confirm"),
            );
        }
        self.verify(&target_ck)?;

        let mut items: BTreeMap<String, Item> = BTreeMap::new();
        for ck in &affected {
            for rel in ck.touched.keys() {
                if !safe_rel(rel) {
                    continue;
                }
                let entry = items.entry(rel.clone());
                let expected = ck.touched.get(rel).cloned();
                match entry {
                    std::collections::btree_map::Entry::Occupied(mut o) => {
                        o.get_mut().expected = expected;
                    }
                    std::collections::btree_map::Entry::Vacant(v) => {
                        let source = if ck.saved.contains(rel) {
                            Source::Saved(fs_backend::dir_for(&self.store, &ck.id).join(rel))
                        } else if ck.created.contains(rel) {
                            Source::Absent
                        } else {
                            continue;
                        };
                        v.insert(Item { expected, source });
                    }
                }
            }
        }
        if policy == RestorePolicy::Full {
            self.add_full_items(&target_ck, &mut items)?;
        }
        let applied = self.apply_plan(&items, policy, &target_ck.id, &mut index);
        fs_backend::remove(&self.store, &format!("{}-full", target_ck.id));
        let paths = applied?;
        // The undone checkpoints are consumed.
        for ck in &affected {
            if let Some(pos) = index.checkpoints.iter().position(|c| c.id == ck.id) {
                let gone = index.checkpoints.remove(pos);
                self.remove_ck(&gone);
            }
        }
        self.trim_redo(&mut index);
        self.meta.save(&index)?;
        Ok(Restored {
            checkpoint: target_ck.id,
            policy,
            files: paths,
        })
    }

    fn verify(&self, ck: &Checkpoint) -> Result<(), ChkError> {
        let (Some(name), Some(tree)) = (&ck.ref_name, &ck.tree_oid) else {
            return Ok(());
        };
        let guard = lock(&self.git);
        let Some(git) = guard.as_ref() else {
            return Ok(());
        };
        match git.ref_tree(name) {
            Some(oid) if oid.to_string() == *tree => Ok(()),
            _ => Err(ChkError::new(
                codes::CHK_HASH,
                format!("checkpoint {} no longer matches its recorded tree", ck.id),
            )
            .recovery("the ref was changed outside Cairn; nothing was restored")),
        }
    }

    /// `Full`: also bring back everything else the checkpoint's tree differs on.
    fn add_full_items(
        &self,
        ck: &Checkpoint,
        items: &mut BTreeMap<String, Item>,
    ) -> Result<(), ChkError> {
        let guard = lock(&self.git);
        let (Some(git), Some(tree)) = (guard.as_ref(), ck.tree_oid.as_deref()) else {
            return Ok(());
        };
        let tree = git2::Oid::from_str(tree).map_err(|e| ChkError::fail(e.to_string()))?;
        let dir = self.store.join("fs").join(format!("{}-full", ck.id));
        for change in git.changes_from_now(tree)? {
            if !safe_rel(&change.path) || items.contains_key(&change.path) {
                continue;
            }
            let source = if change.status == Status::Deleted {
                Source::Absent
            } else {
                let Some((bytes, _)) = git.blob(tree, &change.path) else {
                    continue;
                };
                let dest = dir.join(&change.path);
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p).map_err(|e| ChkError::fail(e.to_string()))?;
                }
                std::fs::write(&dest, bytes).map_err(|e| ChkError::fail(e.to_string()))?;
                Source::Saved(dest)
            };
            items.insert(
                change.path,
                Item {
                    expected: None,
                    source,
                },
            );
        }
        Ok(())
    }

    /// Check, record a redo point, then write. Nothing is written when any
    /// file conflicts.
    fn apply_plan(
        &self,
        items: &BTreeMap<String, Item>,
        policy: RestorePolicy,
        from: &str,
        index: &mut Index,
    ) -> Result<Vec<String>, ChkError> {
        if policy == RestorePolicy::CairnFilesOnly {
            let conflicts: Vec<String> = items
                .iter()
                .filter(|(rel, item)| {
                    item.expected
                        .as_ref()
                        .is_some_and(|want| hash_of(&self.root.join(rel)) != *want)
                })
                .map(|(rel, _)| rel.clone())
                .collect();
            if !conflicts.is_empty() {
                let mut e = ChkError::new(
                    codes::CHK_MERGE,
                    format!("{} file(s) changed since Cairn wrote them", conflicts.len()),
                )
                .recovery("restore anyway with the full policy (it overwrites your edits)");
                e.paths = conflicts;
                return Err(e);
            }
        }
        // Redo point: the current content of everything about to change.
        let seq = index.next_seq;
        index.next_seq += 1;
        let id = format!("ck_{}_redo{seq}", self.short);
        let mut redo = Checkpoint {
            id: id.clone(),
            session: self.session.clone(),
            turn: 0,
            seq,
            label: format!("redo of {from}"),
            created_at: chrono::Utc::now().to_rfc3339(),
            backend: Backend::Fs,
            ref_name: None,
            commit_oid: None,
            tree_oid: None,
            files: 0,
            bytes: 0,
            touched: BTreeMap::new(),
            created: Vec::new(),
            saved: Vec::new(),
            redo_point: true,
        };
        for (rel, item) in items {
            let abs = self.root.join(rel);
            match fs_backend::save(&self.store, &id, rel, &abs) {
                Saved::Copied(n) => {
                    redo.saved.push(rel.clone());
                    redo.bytes += n;
                }
                Saved::Linked => redo.saved.push(rel.clone()),
                Saved::Absent => redo.created.push(rel.clone()),
                Saved::Failed(why) => {
                    return Err(ChkError::fail(format!("cannot save {rel}: {why}")))
                }
            }
            let after = match &item.source {
                Source::Absent => "-".to_string(),
                Source::Saved(p) => std::fs::read(p).map(|b| sha(&b)).map_err(|e| {
                    ChkError::fail(format!("the saved copy of {rel} is unreadable: {e}"))
                })?,
            };
            redo.touched.insert(rel.clone(), after);
        }
        write_items(&self.root, items)?;
        index.checkpoints.push(redo);
        index.redo.entry(self.session.clone()).or_default().push(id);
        Ok(items.keys().cloned().collect())
    }

    fn trim_redo(&self, index: &mut Index) {
        let depth = self.limits.undo_depth;
        let stack = index.redo.entry(self.session.clone()).or_default();
        let excess = stack.len().saturating_sub(depth);
        let old: Vec<String> = stack.drain(..excess).collect();
        for id in old {
            if let Some(pos) = index.checkpoints.iter().position(|c| c.id == id) {
                let ck = index.checkpoints.remove(pos);
                self.remove_ck(&ck);
            }
        }
    }

    /// Re-apply the most recent undo.
    ///
    /// # Errors
    /// `E-CHK-MERGE` when a file changed since the undo; `E-CHK-FAIL` when
    /// there is nothing to redo.
    pub fn redo(&self) -> Result<Restored, ChkError> {
        let _guard = lock(&self.state);
        let mut index = self.meta.load()?;
        let id = index
            .redo
            .get(&self.session)
            .and_then(|s| s.last().cloned())
            .ok_or_else(|| ChkError::fail("there is nothing to redo"))?;
        let ck = index
            .checkpoints
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| ChkError::fail("the redo point is gone"))?;
        let mut items = BTreeMap::new();
        for (rel, want) in &ck.touched {
            if !safe_rel(rel) {
                continue;
            }
            let source = if ck.saved.contains(rel) {
                Source::Saved(fs_backend::dir_for(&self.store, &ck.id).join(rel))
            } else {
                Source::Absent
            };
            items.insert(
                rel.clone(),
                Item {
                    expected: Some(want.clone()),
                    source,
                },
            );
        }
        let conflicts: Vec<String> = items
            .iter()
            .filter(|(rel, item)| {
                item.expected.as_deref() != Some(hash_of(&self.root.join(rel)).as_str())
            })
            .map(|(rel, _)| rel.clone())
            .collect();
        if !conflicts.is_empty() {
            let mut e = ChkError::new(codes::CHK_MERGE, "files changed since the undo");
            e.paths = conflicts;
            return Err(e);
        }
        write_items(&self.root, &items)?;
        if let Some(stack) = index.redo.get_mut(&self.session) {
            stack.pop();
        }
        if let Some(pos) = index.checkpoints.iter().position(|c| c.id == id) {
            let gone = index.checkpoints.remove(pos);
            self.remove_ck(&gone);
        }
        self.meta.save(&index)?;
        Ok(Restored {
            checkpoint: id,
            policy: RestorePolicy::CairnFilesOnly,
            files: items.keys().cloned().collect(),
        })
    }
}

fn write_items(root: &Path, items: &BTreeMap<String, Item>) -> Result<(), ChkError> {
    for (rel, item) in items {
        let abs = root.join(rel);
        let fail = |e: &dyn std::fmt::Display| ChkError::fail(format!("cannot restore {rel}: {e}"));
        match &item.source {
            Source::Absent => match std::fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(fail(&e)),
            },
            Source::Saved(from) => {
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| fail(&e))?;
                }
                let tmp = abs.with_extension("cairn-restore");
                std::fs::copy(from, &tmp).map_err(|e| fail(&e))?;
                std::fs::rename(&tmp, &abs).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    fail(&e)
                })?;
            }
        }
    }
    Ok(())
}

impl WriteObserver for Checkpointer {
    fn checkpoint(&self, label: &str) {
        let turn = self.turn.load(std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = self.begin_turn(turn, label) {
            lock(&self.state).warnings.push(e);
        }
    }

    fn before_write(&self, abs: &Path) {
        let Some(rel) = self.rel_of(abs) else { return };
        let mut st = lock(&self.state);
        let Some(id) = st.active.clone() else { return };
        if st.disabled || st.saved.contains(&rel) || st.created.contains(&rel) {
            return;
        }
        match fs_backend::save(&self.store, &id, &rel, abs) {
            Saved::Copied(n) => {
                st.saved.insert(rel);
                st.bytes += n;
            }
            Saved::Linked => {
                st.saved.insert(rel);
            }
            Saved::Absent => {
                st.created.insert(rel);
            }
            Saved::Failed(why) => {
                st.disabled = true;
                st.warnings
                    .push(ChkError::fail(format!("cannot save {rel}: {why}")));
                return;
            }
        }
        let over = st.bytes > self.limits.fs_max_bytes
            || st.saved.len() + st.created.len() > self.limits.fs_max_files;
        if over && !self.uses_git() {
            st.disabled = true;
            st.warnings.push(
                ChkError::new(
                    codes::CHK_DISK,
                    "too much to checkpoint in a directory without git",
                )
                .recovery("checkpoints are off for the rest of this turn"),
            );
        }
    }

    fn after_write(&self, abs: &Path, sha256: &str) {
        let Some(rel) = self.rel_of(abs) else { return };
        let mut st = lock(&self.state);
        if st.active.is_none() || st.disabled {
            return;
        }
        st.touched.insert(rel, sha256.to_string());
        if let Err(e) = self.flush(&st) {
            st.warnings.push(e);
        }
    }
}
