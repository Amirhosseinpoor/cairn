//! The tree walker (SPEC §5.1): deterministic, never follows symlinks, honours
//! the [`IgnoreEngine`], and stops at its caps instead of running away.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ignore_rules::IgnoreEngine;

/// §5.1 "Max depth".
pub const MAX_DEPTH: usize = 64;
/// §5.1 `discovery.max_entries`.
pub const MAX_ENTRIES: usize = 500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

impl Kind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }
}

/// One thing the walk found.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    /// Relative to the engine's root, `/`-separated.
    pub rel: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    /// Excluded by the ignore rules (only ever `true` when the walk was asked
    /// to list ignored entries; otherwise they are not returned at all).
    pub ignored: bool,
}

/// What a walk produced.
#[derive(Debug, Clone, Default)]
pub struct Walked {
    pub entries: Vec<Entry>,
    /// The entry cap stopped the walk early.
    pub capped: bool,
    /// Symlinks seen and not followed.
    pub symlinks_skipped: usize,
    /// Directories or files that could not be read.
    pub unreadable: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    pub max_depth: usize,
    pub max_entries: usize,
    pub include_hidden: bool,
    pub respect_ignore: bool,
    /// With `respect_ignore` off, still *flag* ignored entries and do not
    /// descend into ignored directories — what `list_dir` shows (§6.2.5).
    pub mark_ignored: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            max_depth: MAX_DEPTH,
            max_entries: MAX_ENTRIES,
            include_hidden: false,
            respect_ignore: true,
            mark_ignored: false,
        }
    }
}

/// The walk could not begin.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("path does not exist")]
    NotFound,
    #[error("cannot read directory: {0}")]
    Unreadable(String),
}

fn rel_of(engine: &IgnoreEngine, path: &Path) -> String {
    path.strip_prefix(engine.root()).map_or_else(
        |_| path.to_string_lossy().replace('\\', "/"),
        |rel| {
            let text = rel.to_string_lossy().replace('\\', "/");
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        },
    )
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Walk below `start`, returning entries in sorted, depth-first order.
///
/// Written by hand rather than on `ignore::WalkBuilder`: the rules here
/// (never follow a symlink, `.git` is never entered, the user's explicit
/// includes admit hidden names, hard caps on depth and entries) are all
/// per-entry decisions that read more plainly as a loop than as filter
/// callbacks, and the order is fixed by construction — names sorted within
/// each directory — rather than by a builder option.
///
/// # Errors
/// [`WalkError`] when `start` does not exist or cannot be listed.
pub fn walk(
    engine: &IgnoreEngine,
    start: &Path,
    options: &WalkOptions,
) -> Result<Walked, WalkError> {
    let meta = std::fs::symlink_metadata(start).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            WalkError::NotFound
        } else {
            WalkError::Unreadable(e.to_string())
        }
    })?;
    let mut walked = Walked::default();
    if !meta.is_dir() {
        walked.entries.push(Entry {
            rel: rel_of(engine, start),
            path: start.to_path_buf(),
            kind: if meta.file_type().is_symlink() {
                Kind::Symlink
            } else {
                Kind::File
            },
            size: meta.len(),
            mtime: meta.modified().ok(),
            ignored: false,
        });
        return Ok(walked);
    }
    std::fs::read_dir(start).map_err(|e| WalkError::Unreadable(e.to_string()))?;
    descend(engine, start, 1, options, &mut walked);
    Ok(walked)
}

/// List one directory and recurse. Returns `false` once the entry cap stops
/// the walk, so every caller up the stack unwinds without more work.
fn descend(
    engine: &IgnoreEngine,
    dir: &Path,
    depth: usize,
    options: &WalkOptions,
    walked: &mut Walked,
) -> bool {
    let Ok(read) = std::fs::read_dir(dir) else {
        walked.unreadable += 1;
        return true;
    };
    let mut children: Vec<std::fs::DirEntry> = read.filter_map(Result::ok).collect();
    children.sort_by_key(std::fs::DirEntry::file_name);
    for child in children {
        let name = child.file_name();
        // `.git` is never entered, whatever the ignore rules or options say.
        if name == ".git" {
            continue;
        }
        let path = child.path();
        let Ok(file_type) = child.file_type() else {
            walked.unreadable += 1;
            continue;
        };
        let is_dir = file_type.is_dir();
        if is_hidden(&name) && !options.include_hidden && !engine.explicitly_included(&path, is_dir)
        {
            continue;
        }
        let ignored =
            (options.respect_ignore || options.mark_ignored) && engine.is_ignored(&path, is_dir);
        if ignored && options.respect_ignore {
            continue;
        }
        if walked.entries.len() >= options.max_entries {
            walked.capped = true;
            return false;
        }
        let kind = if file_type.is_symlink() {
            walked.symlinks_skipped += 1;
            Kind::Symlink
        } else if is_dir {
            Kind::Dir
        } else {
            Kind::File
        };
        let md = child.metadata().ok();
        walked.entries.push(Entry {
            rel: rel_of(engine, &path),
            path: path.clone(),
            kind,
            size: md.as_ref().map_or(0, std::fs::Metadata::len),
            mtime: md.and_then(|m| m.modified().ok()),
            ignored,
        });
        if kind == Kind::Dir
            && !ignored
            && depth < options.max_depth
            && !descend(engine, &path, depth + 1, options, walked)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore_rules::IgnoreOptions;

    fn setup(files: &[&str]) -> (tempfile::TempDir, PathBuf, IgnoreEngine) {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().canonicalize().expect("canonical").join("ws");
        std::fs::create_dir_all(&root).expect("root");
        for f in files {
            let full = root.join(f);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, "x").expect("file");
        }
        let engine = IgnoreEngine::new(&root, &IgnoreOptions::default());
        (dir, root, engine)
    }

    fn rels(w: &Walked) -> Vec<&str> {
        w.entries.iter().map(|e| e.rel.as_str()).collect()
    }

    #[test]
    fn entries_come_back_sorted_and_relative() {
        let (_d, root, engine) = setup(&["b.rs", "a.rs", "src/z.rs", "src/m.rs"]);
        let w = walk(&engine, &root, &WalkOptions::default()).expect("walks");
        assert_eq!(rels(&w), ["a.rs", "b.rs", "src", "src/m.rs", "src/z.rs"]);
        assert!(!w.capped);
    }

    #[test]
    fn ignored_and_hidden_entries_are_skipped_unless_asked_for() {
        let (_d, root, engine) = setup(&[
            ".gitignore",
            "target/x",
            "node_modules/y",
            ".hidden/f",
            ".git/config",
            "keep.rs",
        ]);
        std::fs::write(root.join(".gitignore"), "gen/\n").expect("gitignore");
        std::fs::create_dir_all(root.join("gen")).expect("gen");
        std::fs::write(root.join("gen/out"), "x").expect("out");
        let w = walk(&engine, &root, &WalkOptions::default()).expect("walks");
        assert_eq!(
            rels(&w),
            ["keep.rs"],
            "hidden, ignored and defaults all skipped"
        );

        let all = walk(
            &engine,
            &root,
            &WalkOptions {
                include_hidden: true,
                respect_ignore: false,
                ..WalkOptions::default()
            },
        )
        .expect("walks");
        let found = rels(&all);
        assert!(found.contains(&".gitignore"));
        assert!(found.contains(&".hidden/f"));
        assert!(found.contains(&"gen/out"));
        assert!(found.contains(&"target/x"));
        assert!(
            !found.iter().any(|p| p.starts_with(".git/")),
            ".git is never walked"
        );
    }

    #[test]
    fn a_hidden_directory_the_user_named_is_walked() {
        let (_d, root, _) = setup(&[".github/ci.yml", ".other/x"]);
        std::fs::write(root.join(".cairnignore"), "!.github/\n").expect("cairnignore");
        let engine = IgnoreEngine::new(&root, &IgnoreOptions::default());
        let w = walk(&engine, &root, &WalkOptions::default()).expect("walks");
        assert!(rels(&w).contains(&".github/ci.yml"));
        assert!(!rels(&w).iter().any(|p| p.starts_with(".other")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_listed_but_never_followed() {
        let (_d, root, engine) = setup(&["real/a.rs"]);
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).expect("link");
        let w = walk(&engine, &root, &WalkOptions::default()).expect("walks");
        let found = rels(&w);
        assert!(found.contains(&"alias"));
        assert!(
            !found.contains(&"alias/a.rs"),
            "the link is not descended into"
        );
        assert_eq!(w.symlinks_skipped, 1);
        let alias = w.entries.iter().find(|e| e.rel == "alias").expect("alias");
        assert_eq!(alias.kind, Kind::Symlink);
    }

    #[test]
    fn the_depth_and_entry_caps_stop_the_walk() {
        let (_d, root, engine) = setup(&["a/b/c/d.rs", "a/x.rs", "y.rs", "z.rs"]);
        let shallow = walk(
            &engine,
            &root,
            &WalkOptions {
                max_depth: 1,
                ..WalkOptions::default()
            },
        )
        .expect("walks");
        assert_eq!(rels(&shallow), ["a", "y.rs", "z.rs"]);

        let capped = walk(
            &engine,
            &root,
            &WalkOptions {
                max_entries: 2,
                ..WalkOptions::default()
            },
        )
        .expect("walks");
        assert!(capped.capped);
        assert_eq!(capped.entries.len(), 2);
    }

    #[test]
    fn walking_a_single_file_yields_that_file() {
        let (_d, root, engine) = setup(&["one.rs"]);
        let w = walk(&engine, &root.join("one.rs"), &WalkOptions::default()).expect("walks");
        assert_eq!(rels(&w), ["one.rs"]);
    }

    #[test]
    fn a_missing_start_is_not_found() {
        let (_d, root, engine) = setup(&[]);
        assert!(matches!(
            walk(&engine, &root.join("nope"), &WalkOptions::default()),
            Err(WalkError::NotFound)
        ));
    }

    #[test]
    fn marking_lists_ignored_entries_without_entering_them() {
        let (_d, root, engine) = setup(&["a.rs", "target/deep/x", "node_modules/p/i.js"]);
        let w = walk(
            &engine,
            &root,
            &WalkOptions {
                respect_ignore: false,
                mark_ignored: true,
                ..WalkOptions::default()
            },
        )
        .expect("walks");
        assert_eq!(rels(&w), ["a.rs", "node_modules", "target"]);
        assert!(w
            .entries
            .iter()
            .filter(|e| e.rel != "a.rs")
            .all(|e| e.ignored));
        assert!(
            !w.entries
                .iter()
                .find(|e| e.rel == "a.rs")
                .expect("a")
                .ignored
        );
    }

    #[test]
    fn sizes_and_times_are_recorded() {
        let (_d, root, engine) = setup(&["a.rs"]);
        let w = walk(&engine, &root, &WalkOptions::default()).expect("walks");
        assert_eq!(w.entries[0].size, 1);
        assert!(w.entries[0].mtime.is_some());
    }
}
