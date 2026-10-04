//! The JSONL session store (SPEC §11.7).
//!
//! Layout: `<sessions>/<workspace_sha256_16>/<session_id>.jsonl`, one JSON
//! object per line, UTF-8, mode `0600`, first line a mandatory `header`.
//!
//! Three properties the rest of Cairn leans on:
//!
//! * **a torn last line never loses a session** — a `kill -9` mid-append leaves
//!   a partial final line, which is discarded on read (T-SESS-020,
//!   REQ-ARCH-008);
//! * **unknown records are never destroyed** — they are kept as read and
//!   written back byte-for-byte unless this build edits them (T-SESS-012);
//! * **rewrites are atomic** — temp file, `fsync`, `rename` (REQ-CLI-009).

use crate::record::{corrupt, kind, Header, Record};
use crate::Result;
use cairn_core::error::{codes, CairnError};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// One line of a session file, with the original text kept alongside the
/// parsed record so an untouched line can be written back unchanged.
#[derive(Debug, Clone)]
pub struct Entry {
    pub record: Record,
    /// The line exactly as read (LF-terminated, `\r` stripped).
    pub raw: String,
    /// Set once this build edits `record`; a dirty entry is re-serialized.
    pub dirty: bool,
}

impl Entry {
    #[must_use]
    pub fn new(record: Record) -> Self {
        let raw = serde_json::to_string(&record).unwrap_or_default();
        Self {
            record,
            raw,
            dirty: true,
        }
    }

    /// The line to write: the original while untouched, the record once edited.
    #[must_use]
    pub fn line(&self) -> String {
        if self.dirty {
            serde_json::to_string(&self.record).unwrap_or_else(|_| self.raw.clone())
        } else {
            self.raw.clone()
        }
    }

    /// Mark the record edited (call after mutating [`Entry::record`]).
    pub fn touch(&mut self) {
        self.dirty = true;
    }
}

/// A whole session file in memory.
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub header: Header,
    pub entries: Vec<Entry>,
    /// A partial final line was found and discarded (T-SESS-020).
    pub torn_tail: bool,
}

impl SessionFile {
    /// Every record, in file order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.entries.iter().map(|e| &e.record)
    }

    /// Next free `seq` (SPEC §11.7: monotonic per file).
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|e| e.record.seq)
            .max()
            .map_or(1, |m| m + 1)
    }

    /// Every stored message, oldest first.
    #[must_use]
    pub fn messages(&self) -> Vec<cairn_core::Message> {
        self.entries
            .iter()
            .filter(|e| e.record.kind == kind::MESSAGE)
            .filter_map(|e| e.record.as_message().ok())
            .collect()
    }

    /// The `deleted_at` of a `tombstone`, if the session has been retired.
    #[must_use]
    pub fn tombstoned_at(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|e| {
            (e.record.kind == kind::TOMBSTONE)
                .then(|| e.record.field("deleted_at"))
                .flatten()
                .and_then(|v| v.as_str())
        })
    }

    /// Append a record to the in-memory file (not yet on disk).
    pub fn push(&mut self, record: Record) {
        self.entries.push(Entry::new(record));
    }

    /// The file content, LF-terminated.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(entry.line().as_bytes());
            out.push(b'\n');
        }
        out
    }
}

/// One stored session: the header a listing shows, plus where the bytes are.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
}

/// Options for [`Store::list`].
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Restrict to one workspace root (matched against the stored header).
    pub workspace: Option<PathBuf>,
    /// Case-insensitive substring over the header's JSON.
    pub grep: Option<String>,
    /// Keep at most this many, newest first.
    pub limit: Option<usize>,
    /// Include sessions already carrying a `tombstone`.
    pub include_deleted: bool,
}

/// The session store rooted at `~/.local/share/cairn/sessions` (SPEC §11.6).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `sha256(workspace)` truncated to 16 hex chars (SPEC §11.7 layout).
    #[must_use]
    pub fn workspace_key(workspace: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(workspace.to_string_lossy().as_bytes());
        hex::encode(hasher.finalize())[..16].to_string()
    }

    /// Directory holding one workspace's sessions.
    #[must_use]
    pub fn dir_for(&self, workspace: &Path) -> PathBuf {
        self.root.join(Self::workspace_key(workspace))
    }

    /// Canonical path of one session.
    #[must_use]
    pub fn path_for(&self, workspace: &Path, session_id: &str) -> PathBuf {
        self.dir_for(workspace).join(format!("{session_id}.jsonl"))
    }

    /// Every `.jsonl` file in the store, in directory order. Backups
    /// (`*.jsonl.bak-v<n>`) and temp files never match, so a half-finished
    /// migration cannot be listed as a session.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(dirs) = fs::read_dir(&self.root) else {
            return out;
        };
        for dir in dirs.flatten() {
            let Ok(files) = fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                let p = file.path();
                if p.extension().is_some_and(|e| e == "jsonl") {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    /// Create a session file containing only its `header`.
    ///
    /// Re-creating an id whose stored header already matches is idempotent; an
    /// id that resolves to a *different* session is a collision and fails.
    pub fn create(&self, header: &Header) -> Result<SessionFile> {
        let path = self.path_for(Path::new(&header.workspace), &header.session_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error(&path, &e))?;
        }
        let record = header.to_record();
        match create_exclusive(&path, &record) {
            CreateOutcome::Created => Ok(SessionFile {
                path,
                header: header.clone(),
                entries: vec![Entry::new(record)],
                torn_tail: false,
            }),
            CreateOutcome::Failed(e) => Err(io_error(&path, &e)),
            CreateOutcome::Exists => {
                let existing = self.load(&path)?;
                if existing.header == *header {
                    Ok(existing)
                } else {
                    Err(corrupt(format!(
                        "{} already holds session '{}' from {}",
                        path.display(),
                        existing.header.session_id,
                        existing.header.created_at
                    )))
                }
            }
        }
    }

    /// Append one record as a single LF-terminated line.
    pub fn append(&self, path: &Path, record: &Record) -> Result<()> {
        let line = serde_json::to_string(record)
            .map_err(|e| corrupt(format!("record does not serialize: {e}")))?;
        append_line(path, &line)
    }

    /// Read a whole session. See §11.7's read contract for the three failure
    /// modes (`E-FS-ENCODING`, `E-SESS-CORRUPT`) and the two non-failures.
    pub fn load(&self, path: &Path) -> Result<SessionFile> {
        load_path(path)
    }

    /// Write a session atomically: temp file, `fsync`, `rename`
    /// (REQ-CLI-009, T-SESS-011).
    pub fn save(&self, file: &SessionFile) -> Result<()> {
        atomic_write(&file.path, &file.to_bytes())
    }

    /// Locate a session by id across every workspace in the store.
    #[must_use]
    pub fn find(&self, session_id: &str) -> Option<PathBuf> {
        self.paths()
            .into_iter()
            .find(|p| file_stem(p) == session_id)
    }

    /// Every matching session with the file it lives in, newest first.
    #[must_use]
    pub fn list_all(&self, filter: &ListFilter) -> Vec<Summary> {
        let mut out: Vec<Summary> = Vec::new();
        for path in self.paths() {
            let Ok(first) = first_line(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Record>(&first) else {
                continue;
            };
            if record.kind != kind::HEADER {
                continue;
            }
            let Ok(header) = record.as_header() else {
                continue;
            };
            if let Some(ws) = &filter.workspace {
                if Path::new(&header.workspace) != ws.as_path() {
                    continue;
                }
            }
            if !filter.include_deleted && session_is_retired(&path) {
                continue;
            }
            if let Some(needle) = &filter.grep {
                if !header
                    .search_blob()
                    .to_lowercase()
                    .contains(&needle.to_lowercase())
                {
                    continue;
                }
            }
            out.push(Summary { path, header });
        }
        out.sort_by(|a, b| {
            b.header
                .created_at
                .cmp(&a.header.created_at)
                .then(b.header.session_id.cmp(&a.header.session_id))
        });
        if let Some(n) = filter.limit {
            out.truncate(n);
        }
        out
    }

    /// Headers of every matching session, newest first.
    #[must_use]
    pub fn list(&self, filter: &ListFilter) -> Vec<Header> {
        self.list_all(filter)
            .into_iter()
            .map(|summary| summary.header)
            .collect()
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

/// First line of a file (the `header`), read without loading the rest.
fn first_line(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

/// A `tombstone` as the last record means the session was retired by GC.
///
/// Only the tail is read: a 20 MB session must not cost 20 MB to filter out of
/// a listing. A tombstone is short (§11.7), so it is always wholly inside the
/// window, and a record longer than the window cannot be one.
fn session_is_retired(path: &Path) -> bool {
    const TAIL: u64 = 64 * 1024;
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let mut reader = BufReader::new(file);
    if meta.len() > TAIL {
        use std::io::Seek;
        let offset = i64::try_from(TAIL).unwrap_or(i64::MAX);
        if reader.seek(std::io::SeekFrom::End(-offset)).is_err() {
            return false;
        }
    }
    let mut tail = String::new();
    if reader.read_to_string(&mut tail).is_err() {
        return false;
    }
    tail.lines()
        .rfind(|l| !l.trim().is_empty())
        .is_some_and(|last| last.contains(r#""type":"tombstone""#))
}

/// Outcome of trying to create a session file that already exists.
enum CreateOutcome {
    Created,
    Exists,
    Failed(std::io::Error),
}

/// Create `path` exclusively, writing the header line, mode `0600`.
fn create_exclusive(path: &Path, header: &Record) -> CreateOutcome {
    let line = serde_json::to_string(header).unwrap_or_default();
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');

    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = OpenOptions::new().write(true).create_new(true).open(path);

    let mut file = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return CreateOutcome::Exists,
        Err(e) => return CreateOutcome::Failed(e),
    };
    if let Err(e) = file.write_all(&bytes).and_then(|()| file.flush()) {
        return CreateOutcome::Failed(e);
    }
    CreateOutcome::Created
}

/// Append one LF-terminated line to `path`.
///
/// REQ-LOOP-006: one `write` + `fsync` per record, so a crash loses at most the
/// record being written — never the file's earlier history.
fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| io_error(path, &e))?;
    file.write_all(line.as_bytes())
        .map_err(|e| io_error(path, &e))?;
    file.write_all(b"\n").map_err(|e| io_error(path, &e))?;
    file.flush().map_err(|e| io_error(path, &e))?;
    file.sync_all().map_err(|e| io_error(path, &e))
}

/// Append a line exactly as authored, without reinterpreting it.
///
/// Tests use this to write record types (and field orders) this build does not
/// model, so that preservation claims are tested against bytes we did not
/// produce (T-SESS-012).
#[cfg(test)]
pub(crate) fn append_raw(path: &Path, line: &str) -> Result<()> {
    append_line(path, line)
}

/// Write `bytes` to `path` atomically (SPEC §11.7 REQ-CLI-009).
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_error(path, &e)
    })
}

/// Map an I/O failure onto a stable code (SPEC §11.7, §6.5).
fn io_error(path: &Path, e: &std::io::Error) -> CairnError {
    let (code, hint) = match e.kind() {
        std::io::ErrorKind::NotFound => (
            codes::FS_NOTFOUND,
            "check the path — the session directory may have been moved or deleted",
        ),
        std::io::ErrorKind::PermissionDenied => (
            codes::FS_PERM,
            "session files are mode 0600; check the owner of the file and of its directory",
        ),
        // Everything else (ENOSPC, EROFS, a dying disk) is a write that did
        // not land: REQ-LOOP-006 says a record is `write` + `fsync`, so the
        // failure to report is the flush itself — and at shutdown that is
        // exit 13 (REQ-ARCH-008, §11.2).
        _ => (
            codes::SESS_FLUSH,
            "the record could not be written; earlier records in the file are intact",
        ),
    };
    CairnError::new(code, format!("{}: {}", path.display(), e)).with_recovery(hint)
}

/// Read one session file (SPEC §11.7 read contract).
///
/// Free of [`Store`] on purpose: migration and export operate on a path they
/// already hold, and the rules below depend only on the file.
pub fn load_path(path: &Path) -> Result<SessionFile> {
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CairnError::new(
                codes::SESS_NOTFOUND,
                format!("session file {} does not exist", path.display()),
            )
            .with_recovery("run `cairn sessions` to list stored sessions")
        } else {
            io_error(path, &e)
        }
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        CairnError::new(
            codes::FS_ENCODING,
            format!("{} is not valid UTF-8", path.display()),
        )
        .with_recovery("session files are UTF-8; this one has been overwritten with binary data")
    })?;

    let mut entries: Vec<Entry> = Vec::new();
    let mut torn_tail = false;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(record) => entries.push(Entry {
                record,
                raw: (*line).to_string(),
                dirty: false,
            }),
            Err(_) if i + 1 == lines.len() => {
                // Torn final line: a `kill -9` mid-append (REQ-ARCH-008).
                torn_tail = true;
            }
            Err(e) => {
                return Err(corrupt(format!(
                    "{} line {} is not a session record: {e}",
                    path.display(),
                    i + 1
                )));
            }
        }
    }

    let Some(first) = entries.first() else {
        return Err(corrupt(format!(
            "{} has no `header` record",
            path.display()
        )));
    };
    let header = first.record.as_header()?;
    if file_stem(path) != header.session_id {
        return Err(corrupt(format!(
            "{} is named '{}' but its header says '{}'",
            path.display(),
            file_stem(path),
            header.session_id
        )));
    }
    Ok(SessionFile {
        path: path.to_path_buf(),
        header,
        entries,
        torn_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::CURRENT_SCHEMA_VERSION;
    use std::time::Instant;

    fn header(id: &str, ws: &Path) -> Header {
        Header::new(
            id,
            ws.to_string_lossy(),
            "build",
            "anthropic/claude-sonnet-4-5",
        )
    }

    fn store(tmp: &tempfile::TempDir) -> Store {
        Store::new(tmp.path().join("sessions"))
    }

    fn ws(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("ws")
    }

    #[test]
    fn workspace_key_is_16_hex_chars_and_stable() {
        let a = Store::workspace_key(Path::new("/home/u/proj"));
        let b = Store::workspace_key(Path::new("/home/u/proj"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, Store::workspace_key(Path::new("/home/u/other")));
    }

    #[test]
    fn create_writes_a_header_only_file_mode_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let h = header("ses_a", &ws(&tmp));
        let file = st.create(&h).unwrap();
        let text = std::fs::read_to_string(&file.path).unwrap();
        let first = text.lines().next().unwrap();
        let rec: crate::record::Record = serde_json::from_str(first).unwrap();
        assert_eq!(rec.kind, "header");
        assert_eq!(
            rec.as_header().unwrap().schema_version,
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(text.lines().count(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "session files are 0600 (§11.7)");
        }
    }

    #[test]
    fn re_creating_the_same_header_is_idempotent_but_a_collision_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let h = header("ses_a", &ws(&tmp));
        st.create(&h).unwrap();
        assert!(st.create(&h).is_ok(), "same header → same session");

        let mut other = h.clone();
        other.created_at = "2020-01-01T00:00:00.000Z".into();
        let err = st.create(&other).unwrap_err();
        assert_eq!(err.code, codes::SESS_CORRUPT);
    }

    #[test]
    fn append_then_load_round_trips_untouched_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let file = st.create(&header("ses_a", &ws(&tmp))).unwrap();
        let path = file.path.clone();
        let rec = crate::record::Record::turn_started(1, 1);
        st.append(&path, &rec).unwrap();

        let loaded = st.load(&path).unwrap();
        assert!(!loaded.torn_tail);
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[1].record, rec);
        assert!(!loaded.entries[1].dirty, "a read line is not marked dirty");
        assert_eq!(loaded.next_seq(), 2);
        drop(file);
    }

    #[test]
    fn a_torn_final_line_is_discarded_so_resume_works() {
        // T-SESS-020 / REQ-ARCH-008: `kill -9` lands mid-record.
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let file = st.create(&header("ses_a", &ws(&tmp))).unwrap();
        let mut bytes = std::fs::read(&file.path).unwrap();
        bytes.extend_from_slice(br#"{"v":1,"type":"turn_started","turn_id":2,"seq":2"#);
        std::fs::write(&file.path, bytes).unwrap();

        let loaded = st.load(&file.path).unwrap();
        assert!(loaded.torn_tail);
        assert_eq!(loaded.entries.len(), 1, "the header survives");
        assert_eq!(loaded.header.session_id, "ses_a");
    }

    #[test]
    fn a_corrupt_middle_line_is_an_error_and_leaves_the_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let file = st.create(&header("ses_a", &ws(&tmp))).unwrap();
        st.append(&file.path, &crate::record::Record::turn_started(1, 1))
            .unwrap();
        st.append(&file.path, &crate::record::Record::turn_started(1, 2))
            .unwrap();
        let before = std::fs::read(&file.path).unwrap();

        let mut text = String::from_utf8(before.clone()).unwrap();
        let idx = text.rfind('\n').unwrap();
        text.insert_str(idx, "not json\n");
        std::fs::write(&file.path, text).unwrap();

        let err = st.load(&file.path).unwrap_err();
        assert_eq!(err.code, codes::SESS_CORRUPT, "{}", err.message);
        assert!(err.message.contains("line 3"), "{}", err.message);
        // The failed load MUST NOT rewrite anything (§11.7 read contract).
        let after = std::fs::read(&file.path).unwrap();
        assert!(String::from_utf8_lossy(&after).contains("not json"));
        assert!(!String::from_utf8_lossy(&after).contains(".bak-v"));
    }

    #[test]
    fn a_file_without_a_header_is_not_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let path = tmp.path().join("sessions/x/none.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\n\n").unwrap();
        let err = st.load(&path).unwrap_err();
        assert_eq!(err.code, codes::SESS_CORRUPT);
        assert!(err.message.contains("header"), "{}", err.message);
    }

    #[test]
    fn a_header_that_disagrees_with_the_file_name_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let h = header("ses_a", &ws(&tmp));
        let dir = st.dir_for(&ws(&tmp));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ses_b.jsonl");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&h.to_record()).unwrap()),
        )
        .unwrap();
        let err = st.load(&path).unwrap_err();
        assert_eq!(err.code, codes::SESS_CORRUPT);
        assert!(err.message.contains("ses_a"), "{}", err.message);
    }

    #[test]
    fn a_missing_session_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let err = st
            .load(&tmp.path().join("sessions/nope/ses_x.jsonl"))
            .unwrap_err();
        assert_eq!(err.code, codes::SESS_NOTFOUND);
    }

    #[test]
    fn binary_content_reports_an_encoding_error() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let path = tmp.path().join("sessions/x/ses_y.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert_eq!(st.load(&path).unwrap_err().code, codes::FS_ENCODING);
    }

    #[test]
    fn list_is_newest_first_filtered_and_limited() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let ws = ws(&tmp);
        for (id, at) in [
            ("ses_old", "2026-01-01T00:00:00.000Z"),
            ("ses_new", "2026-06-01T00:00:00.000Z"),
            ("ses_mid", "2026-03-01T00:00:00.000Z"),
        ] {
            let mut h = header(id, &ws);
            h.created_at = at.into();
            st.create(&h).unwrap();
        }

        let all = st.list(&ListFilter::default());
        let ids: Vec<&str> = all.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, ["ses_new", "ses_mid", "ses_old"]);

        let limited = st.list(&ListFilter {
            limit: Some(1),
            ..Default::default()
        });
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].session_id, "ses_new");

        let grepped = st.list(&ListFilter {
            grep: Some("SES_OLD".into()),
            ..Default::default()
        });
        assert_eq!(grepped.len(), 1, "grep is case-insensitive");
        assert_eq!(grepped[0].session_id, "ses_old");

        let other = Path::new("/somewhere/else");
        let filtered = st.list(&ListFilter {
            workspace: Some(other.to_path_buf()),
            ..Default::default()
        });
        assert!(filtered.is_empty());
    }

    #[test]
    fn retired_sessions_are_hidden_from_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let h = header("ses_gone", &ws(&tmp));
        let mut file = st.create(&h).unwrap();
        file.push(Record::tombstone("retention"));
        st.save(&file).unwrap();

        assert!(st.list(&ListFilter::default()).is_empty());
        assert_eq!(
            st.list(&ListFilter {
                include_deleted: true,
                ..Default::default()
            })
            .len(),
            1
        );
        assert!(st.load(&file.path).unwrap().tombstoned_at().is_some());
    }

    #[test]
    fn save_is_atomic_and_preserves_untouched_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let file = st.create(&header("ses_a", &ws(&tmp))).unwrap();
        st.append(&file.path, &Record::turn_started(1, 1)).unwrap();
        let mut reloaded = st.load(&file.path).unwrap();
        let raw_before = reloaded.entries[1].raw.clone();

        reloaded.push(Record::turn_started(1, 2));
        st.save(&reloaded).unwrap();

        let saved = st.load(&file.path).unwrap();
        assert_eq!(saved.entries.len(), 3);
        assert_eq!(
            saved.entries[1].raw, raw_before,
            "untouched line is byte-identical"
        );
        assert!(!file.path.with_extension("jsonl.tmp").exists());
    }

    #[test]
    fn a_20_mb_session_loads_within_the_p19_budget() {
        // T-SESS-010: 5,000 records / ~20 MB → resume ≤ 400 ms.
        let tmp = tempfile::tempdir().unwrap();
        let st = store(&tmp);
        let mut file = st.create(&header("ses_big", &ws(&tmp))).unwrap();
        let filler = "x".repeat(4096);
        for i in 0..5000u64 {
            file.push(Record::new(
                crate::record::kind::TOOL_RESULT,
                Some(i + 1),
                serde_json::json!({
                    "turn_id": i, "call_id": format!("c{i}"), "name": "read_file",
                    "ok": true, "output": filler, "duration_ms": 1, "truncated": false
                }),
            ));
        }
        st.save(&file).unwrap();
        let size = std::fs::metadata(&file.path).unwrap().len();
        assert!(size > 20_000_000, "fixture is {size} bytes");

        let started = Instant::now();
        let loaded = st.load(&file.path).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(loaded.entries.len(), 5001);
        assert!(
            elapsed.as_millis() <= 400,
            "load took {elapsed:?}, budget 400 ms (P-19)"
        );
    }
}
