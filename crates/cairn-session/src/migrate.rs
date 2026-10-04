//! Forward-only session migration (SPEC §11.7, REQ-CLI-009).
//!
//! `header.schema_version` older than [`CURRENT_SCHEMA_VERSION`] is upgraded by
//! [`migrate_file`], which:
//!
//! 1. copies the original to `<id>.jsonl.bak-v<from>` **first**,
//! 2. applies every registered step in order,
//! 3. writes `<id>.jsonl.tmp` and `rename`s it over the original.
//!
//! Nothing downgrades (§0 non-goals): a file from a *newer* Cairn is reported
//! as such and left byte-identical, because §11.7's forward-compatibility rules
//! already let this build read it. Records whose `type` this build does not
//! know are carried through untouched — that is what makes it safe for an older
//! Cairn to rewrite a file a newer one wrote.
//!
//! A step that is *not* registered is a content-preserving version bump: the
//! version number is the only thing that changes. That is how the machinery is
//! exercised against a hypothetical next version in `T-SESS-011` without
//! pretending a migration exists that does not.

use crate::record::{Header, CURRENT_SCHEMA_VERSION};
use crate::store::{load_path, SessionFile};
use crate::Result;
use std::path::{Path, PathBuf};

/// What [`migrate_file`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Already at the requested version.
    UpToDate,
    /// Rewritten at a higher version.
    Migrated,
    /// Written by a newer Cairn; forward-only policy leaves it alone.
    NewerThanTarget,
}

/// Outcome of migrating one file.
#[derive(Debug, Clone)]
pub struct MigrateReport {
    pub path: PathBuf,
    pub session_id: String,
    pub from: u32,
    pub to: u32,
    pub action: Action,
    /// Where the pre-migration bytes were saved (only when `Migrated`).
    pub backup: Option<PathBuf>,
    /// Human-readable notes, one per applied or skipped version step.
    pub notes: Vec<String>,
}

/// One registered version step.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub from: u32,
    pub to: u32,
    pub apply: fn(&mut SessionFile),
    pub note: &'static str,
}

/// `schema_version` 0 → 1: the pre-versioned format.
///
/// The only content change is stamping the version: `header.schema_version`
/// becomes 1 and any record that arrived without a `v` gets one. Everything
/// else — field order, unknown records — is left exactly as found.
pub fn v0_to_v1(file: &mut SessionFile) {
    file.header.schema_version = 1;
    for entry in file.entries.iter_mut().skip(1) {
        if entry.record.v < 1 {
            entry.record.v = 1;
            entry.touch();
        }
    }
}

/// Every migration this build knows (SPEC §11.7). Extend by appending; never
/// edit or reorder an entry that has shipped.
pub const MIGRATIONS: &[Step] = &[Step {
    from: 0,
    to: 1,
    apply: v0_to_v1,
    note: "0→1: stamp header.schema_version and per-record v",
}];

/// Migrate one session file to `target`.
pub fn migrate_file(path: &Path, target: u32) -> Result<MigrateReport> {
    let mut file = load_path(path)?;
    let session_id = file.header.session_id.clone();
    let from = file.header.schema_version;

    let action = match from.cmp(&target) {
        std::cmp::Ordering::Equal => Action::UpToDate,
        std::cmp::Ordering::Greater => Action::NewerThanTarget,
        std::cmp::Ordering::Less => Action::Migrated,
    };

    let mut notes = Vec::new();
    if action != Action::Migrated {
        notes.push(match action {
            Action::UpToDate => format!("schema_version {from} is current; nothing to do"),
            Action::NewerThanTarget => format!(
                "schema_version {from} is newer than this build's {target}; left byte-identical (no downgrades, §0)"
            ),
            Action::Migrated => unreachable!(),
        });
        return Ok(MigrateReport {
            path: path.to_path_buf(),
            session_id,
            from,
            to: from,
            action,
            backup: None,
            notes,
        });
    }

    // 1. Backup first: the original bytes must survive any later failure.
    let original = std::fs::read(path).map_err(|e| {
        crate::record::corrupt(format!("{} cannot be read for backup: {e}", path.display()))
    })?;
    let backup = path.with_extension(format!("jsonl.bak-v{from}"));
    crate::store::atomic_write(&backup, &original)?;

    // 2. Apply every step from `from` to `target`, in order.
    let mut version = from;
    while version < target {
        if let Some(step) = MIGRATIONS.iter().find(|s| s.from == version) {
            (step.apply)(&mut file);
            notes.push(step.note.to_string());
            version = step.to;
        } else {
            notes.push(format!(
                "v{version}→v{}: no content migration registered; version stamp only",
                version + 1
            ));
            version += 1;
        }
    }

    // 3. Publish the new header version, then rewrite atomically.
    file.header.schema_version = target;
    sync_header(&mut file);
    crate::store::atomic_write(path, &file.to_bytes())?;

    // The rewrite is only "done" if it still loads — a migration that produces
    // an unloadable file is a defect, not a success.
    let check = load_path(path)?;
    debug_assert_eq!(check.header.schema_version, target);

    notes.push(format!("backup at {}", backup.display()));
    Ok(MigrateReport {
        path: path.to_path_buf(),
        session_id,
        from,
        to: target,
        action: Action::Migrated,
        backup: Some(backup),
        notes,
    })
}

/// Write `file.header` back into the header record without discarding fields
/// this build's `Header` type does not model.
fn sync_header(file: &mut SessionFile) {
    let Ok(value) = serde_json::to_value(&file.header) else {
        return;
    };
    let Some(map) = value.as_object() else {
        return;
    };
    let Some(entry) = file.entries.first_mut() else {
        return;
    };
    let mut changed = false;
    for (key, want) in map {
        if entry.record.body.get(key) != Some(want) {
            entry.record.body.insert(key.clone(), want.clone());
            changed = true;
        }
    }
    // Optional fields the header no longer carries must not linger.
    for key in ["ruleset_version", "parent_session", "plan_id"] {
        if !map.contains_key(key) && entry.record.body.remove(key).is_some() {
            changed = true;
        }
    }
    if changed {
        entry.touch();
    }
}

/// Migrate every session in the store. Corrupt files are reported, never
/// rewritten, so one bad file cannot stop a maintenance run.
#[must_use]
pub fn migrate_all(
    paths: &[PathBuf],
    target: u32,
) -> (
    Vec<MigrateReport>,
    Vec<(PathBuf, cairn_core::error::CairnError)>,
) {
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for path in paths {
        match migrate_file(path, target) {
            Ok(report) => ok.push(report),
            Err(e) => failed.push((path.clone(), e)),
        }
    }
    (ok, failed)
}

/// The `schema_version` of a session file, or `None` if it is not a session.
#[must_use]
pub fn peek_schema_version(path: &Path) -> Option<u32> {
    load_path(path).ok().map(|f| f.header.schema_version)
}

/// Convenience for callers that already hold a header.
#[must_use]
pub fn needs_migration(header: &Header) -> bool {
    header.schema_version < CURRENT_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{kind, Record};
    use crate::store::{ListFilter, Store};

    /// A session file already at `version`, so a test can name the migration
    /// it actually wants to exercise.
    fn fixture(tmp: &tempfile::TempDir, version: u32) -> (Store, PathBuf) {
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let mut header = Header::new("ses_m", ws.to_string_lossy(), "build", "m");
        header.schema_version = version;
        let file = store.create(&header).unwrap();
        let path = file.path.clone();
        (store, path)
    }

    #[test]
    fn a_v0_file_is_migrated_with_a_backup_written_first() {
        // T-SESS-011 (REQ-CLI-009): backup, then an atomic rename.
        let tmp = tempfile::tempdir().unwrap();
        let (_store, path) = fixture(&tmp, 0);
        let before = std::fs::read(&path).unwrap();

        let report = migrate_file(&path, 1).unwrap();
        assert_eq!(report.action, Action::Migrated);
        assert_eq!((report.from, report.to), (0, 1));
        let backup = report.backup.unwrap();
        assert_eq!(backup, path.with_extension("jsonl.bak-v0"));
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            before,
            "backup is the original"
        );
        assert!(
            !path.with_extension("jsonl.tmp").exists(),
            "temp file is gone"
        );

        let reloaded = load_path(&path).unwrap();
        assert_eq!(reloaded.header.schema_version, 1);
        assert_eq!(reloaded.entries[0].record.kind, kind::HEADER);
    }

    #[test]
    fn a_hypothetical_v2_is_a_pure_version_bump() {
        // T-SESS-011's "v1 file with a hypothetical v2 feature": the machinery
        // must run the same backup + atomic rename with no content step.
        let tmp = tempfile::tempdir().unwrap();
        let (store, path) = fixture(&tmp, 1);
        store.append(&path, &Record::turn_started(1, 1)).unwrap();
        let before_entries = load_path(&path).unwrap().entries.len();

        let report = migrate_file(&path, 2).unwrap();
        assert_eq!(report.action, Action::Migrated);
        assert_eq!((report.from, report.to), (1, 2));
        assert!(report.backup.unwrap().ends_with("ses_m.jsonl.bak-v1"));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("version stamp only")),
            "{:?}",
            report.notes
        );

        let after = load_path(&path).unwrap();
        assert_eq!(after.header.schema_version, 2);
        assert_eq!(
            after.entries.len(),
            before_entries,
            "no records added or lost"
        );
        assert_eq!(
            after.records().last().unwrap().seq,
            Some(1),
            "seq order kept"
        );
    }

    #[test]
    fn unknown_record_types_survive_migration_verbatim() {
        // T-SESS-012 (REQ-CLI-009).
        let tmp = tempfile::tempdir().unwrap();
        let (_store, path) = fixture(&tmp, 0);
        let future = r#"{"v":7,"type":"quantum_record","seq":4,"payload":{"a":[1,2]}}"#;
        crate::store::append_raw(&path, future).unwrap();

        migrate_file(&path, 1).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#"{"v":7,"type":"quantum_record","seq":4,"payload":{"a":[1,2]}}"#),
            "the unknown record must be byte-identical:\n{text}"
        );
    }

    #[test]
    fn a_current_file_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let file = store
            .create(&Header::new("ses_now", ws.to_string_lossy(), "build", "m"))
            .unwrap();
        let before = std::fs::read(&file.path).unwrap();

        let report = migrate_file(&file.path, CURRENT_SCHEMA_VERSION).unwrap();
        assert_eq!(report.action, Action::UpToDate);
        assert!(report.backup.is_none());
        assert_eq!(
            std::fs::read(&file.path).unwrap(),
            before,
            "no rewrite at all"
        );
    }

    #[test]
    fn a_newer_file_is_never_downgraded() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, path) = fixture(&tmp, 1);
        migrate_file(&path, 2).unwrap();
        let before = std::fs::read(&path).unwrap();

        let report = migrate_file(&path, 1).unwrap();
        assert_eq!(report.action, Action::NewerThanTarget);
        assert!(report.backup.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(store.list(&ListFilter::default()).len(), 1);
    }

    #[test]
    fn migrate_all_keeps_going_after_a_bad_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, good) = fixture(&tmp, 0);
        let bad = tmp.path().join("sessions/x/bad.jsonl");
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, "not json\nstill not json\n").unwrap();

        let (ok, failed) = migrate_all(&[good.clone(), bad.clone()], 1);
        assert_eq!(ok.len(), 1);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, bad);
        assert_eq!(failed[0].1.code, cairn_core::error::codes::SESS_CORRUPT);
        assert_eq!(store.list(&ListFilter::default()).len(), 1);
    }

    #[test]
    fn needs_migration_only_looks_at_the_header() {
        let mut h = Header::new("s", "/w", "build", "m");
        assert!(!needs_migration(&h));
        h.schema_version = 0;
        assert!(needs_migration(&h));
        assert_eq!(peek_schema_version(Path::new("/nope.jsonl")), None);
    }
}
