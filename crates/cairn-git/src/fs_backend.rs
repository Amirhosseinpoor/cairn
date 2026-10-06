//! The copy half: pre-images of the files Cairn is about to write.

use std::path::{Path, PathBuf};

/// Files up to this size are copied; larger ones are hard-linked (writers
/// replace by rename, so the old inode keeps the old bytes).
pub const COPY_LIMIT: u64 = 8 * 1024 * 1024;

pub fn dir_for(store: &Path, id: &str) -> PathBuf {
    store.join("fs").join(id)
}

pub enum Saved {
    Copied(u64),
    Linked,
    Absent,
    Failed(String),
}

/// Save the current content of `abs` under `<store>/fs/<id>/<rel>`.
pub fn save(store: &Path, id: &str, rel: &str, abs: &Path) -> Saved {
    let Ok(meta) = std::fs::metadata(abs) else {
        return Saved::Absent;
    };
    if !meta.is_file() {
        return Saved::Absent;
    }
    let dest = dir_for(store, id).join(rel);
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Saved::Failed(e.to_string());
        }
    }
    if meta.len() <= COPY_LIMIT {
        match std::fs::copy(abs, &dest) {
            Ok(n) => Saved::Copied(n),
            Err(e) => Saved::Failed(e.to_string()),
        }
    } else {
        match std::fs::hard_link(abs, &dest) {
            Ok(()) => Saved::Linked,
            Err(e) => Saved::Failed(format!("cannot link {}: {e}", abs.display())),
        }
    }
}

pub fn remove(store: &Path, id: &str) {
    let _ = std::fs::remove_dir_all(dir_for(store, id));
}
