//! The SQLite index: scanning a workspace into it, keeping it current, and
//! ranking files out of it (SPEC §5.2, §5.3).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cairn_search::{classify, walk, Content, IgnoreEngine, Kind, WalkOptions, SNIFF_BYTES};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::extract::{extract, Extracted, Language};
use crate::rank::{self, Corpus, Document, Edge, Params};
use crate::resolve::Resolver;

/// A file this big is not read for the index (§5.1).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// §5.2: how many files a map holds.
pub const TOP_K: usize = 40;
/// Bump to rebuild every index on disk (§5.3 rule 4).
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  language TEXT,
  size INTEGER,
  mtime_ns INTEGER,
  sha256 BLOB NOT NULL,
  status TEXT NOT NULL
);
CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  simple_name TEXT NOT NULL,
  kind TEXT NOT NULL,
  line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  container TEXT,
  signature TEXT,
  doc TEXT
);
CREATE INDEX idx_symbols_name ON symbols(simple_name);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE TABLE edges (
  from_file INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  to_file INTEGER NOT NULL,
  kind TEXT NOT NULL,
  weight REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (from_file, to_file, kind)
);
CREATE TABLE imports (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  spec TEXT NOT NULL
);
CREATE INDEX idx_imports_file ON imports(file_id);
CREATE TABLE refs (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  n INTEGER NOT NULL
);
CREATE INDEX idx_refs_file ON refs(file_id);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Something the index could not do.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(String),
}

type Result<T> = std::result::Result<T, IndexError>;

/// Where a workspace's index lives: `<cache>/index/<sha256(root + salt)>.sqlite3`.
#[must_use]
pub fn cache_path(cache_home: &Path, workspace: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    hasher.update(b"\0cairn-index-v1");
    cache_home
        .join("index")
        .join(format!("{}.sqlite3", hex::encode(hasher.finalize())))
}

/// What a scan was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    pub max_file_bytes: u64,
    pub walk: WalkOptions,
    /// Hash every file, trusting neither size nor mtime (§5.3 rule 5).
    pub verify: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: MAX_FILE_BYTES,
            walk: WalkOptions::default(),
            verify: false,
        }
    }
}

/// What a scan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanReport {
    /// Files seen by the walk.
    pub seen: usize,
    /// Parsed this time (new or changed).
    pub parsed: usize,
    /// Skipped on size and mtime alone.
    pub unchanged: usize,
    /// Hashed and found identical.
    pub rehashed: usize,
    pub removed: usize,
    pub binary: usize,
    pub too_large: usize,
    /// Files the parser could not read at all (`status = 'error'`).
    pub errors: usize,
    /// The walk's entry cap stopped it (`W-DISC-CAP`).
    pub capped: bool,
    pub millis: u128,
}

/// A symbol as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub name: String,
    pub simple_name: String,
    pub kind: String,
    pub line: u32,
    pub end_line: u32,
    pub container: Option<String>,
    pub signature: String,
    pub doc: String,
}

/// Counts for `doctor` and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
}

/// What to rank for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<'a> {
    pub text: &'a str,
    pub current_file: Option<&'a str>,
    pub touched: &'a [String],
    pub top_k: usize,
}

/// One entry of the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub path: String,
    /// The §5.2 `final` score, `[0, 1]`.
    pub score: f64,
    /// Lines of symbols whose names matched the query (REQ-CTX-006).
    pub lines_of_interest: Vec<u32>,
    pub symbols: Vec<SymbolRow>,
}

/// Everything ranking needs, loaded once per index generation.
struct Snapshot {
    paths: Vec<String>,
    edges: Vec<Edge>,
    symbols: Vec<Vec<SymbolRow>>,
    corpus: Corpus,
}

/// The workspace index.
pub struct Index {
    conn: Mutex<Connection>,
    root: PathBuf,
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    ready: AtomicBool,
    params: Mutex<Params>,
}

impl std::fmt::Debug for Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn mtime_ns(time: Option<SystemTime>) -> i64 {
    time.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

impl Index {
    /// Open (or create) the index at `path` for `root`. A file from another
    /// schema version, or one that is not a database, is replaced (§5.3).
    ///
    /// # Errors
    /// [`IndexError`] when the file cannot be created.
    pub fn open(path: &Path, root: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| IndexError::Io(e.to_string()))?;
        }
        let fresh = |path: &Path| -> Result<Connection> {
            for suffix in ["", "-wal", "-shm"] {
                let mut name = path.as_os_str().to_os_string();
                name.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(name));
            }
            let conn = Connection::open(path)?;
            init(&conn)?;
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            Ok(conn)
        };
        let conn = match Connection::open(path).and_then(|c| {
            init(&c)?;
            let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            Ok((c, v))
        }) {
            Ok((c, SCHEMA_VERSION)) => c,
            Ok((c, 0)) => {
                // A new, empty file.
                c.execute_batch(SCHEMA)?;
                c.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                c
            }
            Ok((c, _)) => {
                drop(c);
                fresh(path)?
            }
            Err(_) => fresh(path)?,
        };
        Ok(Self::with(conn, root))
    }

    /// An index that lives in memory, for tests.
    ///
    /// # Errors
    /// [`IndexError`] if SQLite cannot open it.
    pub fn open_in_memory(root: &Path) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self::with(conn, root))
    }

    fn with(conn: Connection, root: &Path) -> Self {
        let populated = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n > 0);
        Self {
            conn: Mutex::new(conn),
            root: root.to_path_buf(),
            snapshot: Mutex::new(None),
            ready: AtomicBool::new(populated),
            params: Mutex::new(Params::default()),
        }
    }

    /// Use these ranking numbers from now on (`repo_map.*`).
    pub fn set_params(&self, params: Params) {
        *lock(&self.params) = params;
        *lock(&self.snapshot) = None;
    }

    /// REQ-CTX-009: `true` until a first scan has finished, while a fresh
    /// index is not yet usable and callers fall back to `glob` and `grep`.
    #[must_use]
    pub fn degraded(&self) -> bool {
        !self.ready.load(Ordering::Relaxed)
    }

    /// Row counts.
    ///
    /// # Errors
    /// [`IndexError::Db`].
    pub fn stats(&self) -> Result<Stats> {
        let conn = lock(&self.conn);
        let count = |table: &str| -> Result<usize> {
            let n: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            Ok(usize::try_from(n).unwrap_or(0))
        };
        Ok(Stats {
            files: count("files")?,
            symbols: count("symbols")?,
            edges: count("edges")?,
        })
    }

    /// The symbols of one file, in line order.
    ///
    /// # Errors
    /// [`IndexError::Db`].
    pub fn symbols(&self, rel: &str) -> Result<Vec<SymbolRow>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT s.name, s.simple_name, s.kind, s.line, s.end_line, s.container, s.signature, s.doc
             FROM symbols s JOIN files f ON f.id = s.file_id WHERE f.path = ?1 ORDER BY s.line, s.id",
        )?;
        let rows = stmt.query_map([rel], row_to_symbol)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Edges as `(from path, to path, kind)`.
    ///
    /// # Errors
    /// [`IndexError::Db`].
    pub fn edges(&self) -> Result<Vec<(String, String, String)>> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT a.path, b.path, e.kind FROM edges e
             JOIN files a ON a.id = e.from_file JOIN files b ON b.id = e.to_file
             ORDER BY a.path, b.path, e.kind",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// The status recorded for `rel`.
    ///
    /// # Errors
    /// [`IndexError::Db`].
    pub fn status_of(&self, rel: &str) -> Result<Option<String>> {
        let conn = lock(&self.conn);
        Ok(conn
            .query_row("SELECT status FROM files WHERE path = ?1", [rel], |r| {
                r.get(0)
            })
            .optional()?)
    }
}

fn row_to_symbol(r: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        name: r.get(0)?,
        simple_name: r.get(1)?,
        kind: r.get(2)?,
        line: r.get(3)?,
        end_line: r.get(4)?,
        container: r.get(5)?,
        signature: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
        doc: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
    })
}

/// What was learned about one file.
struct Read {
    rel: String,
    size: i64,
    mtime_ns: i64,
    sha: Vec<u8>,
    status: &'static str,
    language: Option<Language>,
    extracted: Option<Extracted>,
    /// Same bytes as before: only the stat fields change.
    same: bool,
}

struct Known {
    id: i64,
    size: i64,
    mtime_ns: i64,
    sha: Vec<u8>,
}

fn read_file(
    path: &Path,
    rel: &str,
    size: u64,
    mtime: i64,
    max: u64,
    known: Option<&Known>,
) -> Read {
    let mut read = Read {
        rel: rel.to_string(),
        size: i64::try_from(size).unwrap_or(i64::MAX),
        mtime_ns: mtime,
        sha: Vec::new(),
        status: "indexed",
        language: Language::detect(rel),
        extracted: None,
        same: false,
    };
    if size > max {
        read.status = "too_large";
        read.language = None;
        return read;
    }
    let Ok(bytes) = std::fs::read(path) else {
        read.status = "error";
        return read;
    };
    read.sha = Sha256::digest(&bytes).to_vec();
    if known.is_some_and(|k| k.sha == read.sha) {
        read.same = true;
        return read;
    }
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if !matches!(classify(sample, size), Content::Text { .. }) {
        read.status = "binary";
        read.language = None;
        return read;
    }
    if let Some(lang) = read.language {
        match std::str::from_utf8(&bytes) {
            Ok(text) => match extract(lang, text) {
                Ok(found) => read.extracted = Some(found),
                Err(_) => read.status = "error",
            },
            Err(_) => read.status = "error",
        }
    }
    read
}

impl Index {
    /// Walk the workspace and bring the index up to date (§5.3): files whose
    /// size and mtime are unchanged are skipped, files whose bytes are
    /// unchanged only get new stat fields, the rest are parsed on the rayon
    /// pool, and rows for files that are gone are removed.
    ///
    /// # Errors
    /// [`IndexError`] for database or filesystem failures; a file that cannot
    /// be parsed is a row with `status = 'error'`, not an error (REQ-CTX-008).
    pub fn scan(&self, engine: &IgnoreEngine, options: &ScanOptions) -> Result<ScanReport> {
        let started = Instant::now();
        let walked = walk(engine, &self.root, &options.walk)
            .map_err(|e| IndexError::Io(format!("{e:?}")))?;
        let mut report = ScanReport {
            capped: walked.capped,
            ..ScanReport::default()
        };
        let files: Vec<_> = walked
            .entries
            .iter()
            .filter(|e| e.kind == Kind::File)
            .collect();
        report.seen = files.len();

        let known: HashMap<String, Known> = {
            let conn = lock(&self.conn);
            let mut stmt = conn.prepare("SELECT id, path, size, mtime_ns, sha256 FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    Known {
                        id: r.get(0)?,
                        size: r.get(2)?,
                        mtime_ns: r.get(3)?,
                        sha: r.get(4)?,
                    },
                ))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        let todo: Vec<_> = files
            .iter()
            .filter(|e| {
                let size = i64::try_from(e.size).unwrap_or(i64::MAX);
                match known.get(&e.rel) {
                    Some(k)
                        if !options.verify && k.size == size && k.mtime_ns == mtime_ns(e.mtime) =>
                    {
                        report.unchanged += 1;
                        false
                    }
                    _ => true,
                }
            })
            .collect();
        let reads: Vec<Read> = todo
            .par_iter()
            .map(|e| {
                read_file(
                    &e.path,
                    &e.rel,
                    e.size,
                    mtime_ns(e.mtime),
                    options.max_file_bytes,
                    known.get(&e.rel),
                )
            })
            .collect();

        let present: BTreeSet<&str> = files.iter().map(|e| e.rel.as_str()).collect();
        let gone: Vec<i64> = known
            .iter()
            .filter(|(path, _)| !present.contains(path.as_str()))
            .map(|(_, k)| k.id)
            .collect();
        report.removed = gone.len();

        let mut changed = !gone.is_empty();
        {
            let mut conn = lock(&self.conn);
            let tx = conn.transaction()?;
            for id in &gone {
                tx.execute("DELETE FROM files WHERE id = ?1", [id])?;
            }
            for read in &reads {
                if read.same {
                    report.rehashed += 1;
                    tx.execute(
                        "UPDATE files SET size = ?2, mtime_ns = ?3 WHERE path = ?1",
                        params![read.rel, read.size, read.mtime_ns],
                    )?;
                    continue;
                }
                changed = true;
                match read.status {
                    "binary" => report.binary += 1,
                    "too_large" => report.too_large += 1,
                    "error" => report.errors += 1,
                    _ => report.parsed += 1,
                }
                tx.execute("DELETE FROM files WHERE path = ?1", [&read.rel])?;
                tx.execute(
                    "INSERT INTO files (path, language, size, mtime_ns, sha256, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![read.rel, read.language.map(Language::name), read.size, read.mtime_ns, read.sha, read.status],
                )?;
                let id = tx.last_insert_rowid();
                if let Some(found) = &read.extracted {
                    store_extracted(&tx, id, found)?;
                }
            }
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('last_scan', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [chrono_like_now()],
            )?;
            tx.commit()?;
        }
        if changed {
            self.rebuild_edges()?;
        }
        *lock(&self.snapshot) = None;
        self.ready.store(true, Ordering::Relaxed);
        report.millis = started.elapsed().as_millis();
        Ok(report)
    }

    /// Re-read only `paths` (absolute, from the file watcher), then rebuild
    /// edges. A path that no longer exists is removed.
    ///
    /// # Errors
    /// [`IndexError`] for database failures.
    pub fn refresh(&self, paths: &[PathBuf], options: &ScanOptions) -> Result<ScanReport> {
        let started = Instant::now();
        let mut report = ScanReport::default();
        let mut changed = false;
        let mut conn = lock(&self.conn);
        let tx = conn.transaction()?;
        for abs in paths {
            let Ok(rel) = abs.strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            report.seen += 1;
            let meta = std::fs::metadata(abs)
                .ok()
                .filter(std::fs::Metadata::is_file);
            let Some(meta) = meta else {
                if tx.execute("DELETE FROM files WHERE path = ?1", [&rel])? > 0 {
                    report.removed += 1;
                    changed = true;
                }
                continue;
            };
            let known = tx
                .query_row(
                    "SELECT id, size, mtime_ns, sha256 FROM files WHERE path = ?1",
                    [&rel],
                    |r| {
                        Ok(Known {
                            id: r.get(0)?,
                            size: r.get(1)?,
                            mtime_ns: r.get(2)?,
                            sha: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            let read = read_file(
                abs,
                &rel,
                meta.len(),
                mtime_ns(meta.modified().ok()),
                options.max_file_bytes,
                known.as_ref(),
            );
            if read.same {
                report.rehashed += 1;
                tx.execute(
                    "UPDATE files SET size = ?2, mtime_ns = ?3 WHERE path = ?1",
                    params![read.rel, read.size, read.mtime_ns],
                )?;
                continue;
            }
            changed = true;
            report.parsed += 1;
            tx.execute("DELETE FROM files WHERE path = ?1", [&read.rel])?;
            tx.execute(
                "INSERT INTO files (path, language, size, mtime_ns, sha256, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![read.rel, read.language.map(Language::name), read.size, read.mtime_ns, read.sha, read.status],
            )?;
            let id = tx.last_insert_rowid();
            if let Some(found) = &read.extracted {
                store_extracted(&tx, id, found)?;
            }
        }
        tx.commit()?;
        drop(conn);
        if changed {
            self.rebuild_edges()?;
            *lock(&self.snapshot) = None;
        }
        report.millis = started.elapsed().as_millis();
        Ok(report)
    }

    /// Recompute every edge from the stored imports and references.
    fn rebuild_edges(&self) -> Result<()> {
        let mut conn = lock(&self.conn);
        let files: Vec<(i64, String, Option<String>)> = {
            let mut stmt = conn.prepare("SELECT id, path, language FROM files ORDER BY path")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let id_of: HashMap<&str, i64> = files.iter().map(|(id, p, _)| (p.as_str(), *id)).collect();
        let resolver = Resolver::new(files.iter().map(|(_, p, _)| p.clone()));
        let language_of: HashMap<i64, Language> = files
            .iter()
            .filter_map(|(id, _, lang)| {
                let lang = lang.as_deref()?;
                Language::ALL
                    .into_iter()
                    .find(|l| l.name() == lang)
                    .map(|l| (*id, l))
            })
            .collect();
        let path_of: HashMap<i64, &str> =
            files.iter().map(|(id, p, _)| (*id, p.as_str())).collect();

        let mut imports: HashMap<i64, Vec<String>> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT file_id, spec FROM imports")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, spec) = row?;
                imports.entry(id).or_default().push(spec);
            }
        }
        let mut by_name: HashMap<String, BTreeSet<i64>> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT DISTINCT simple_name, file_id FROM symbols")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (name, id) = row?;
                by_name.entry(name).or_default().insert(id);
            }
        }
        let mut refs: HashMap<i64, Vec<(String, String)>> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT file_id, name, kind FROM refs")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (id, name, kind) = row?;
                refs.entry(id).or_default().push((name, kind));
            }
        }

        let mut edges: BTreeMap<(i64, i64, &'static str), f64> = BTreeMap::new();
        for (id, lang) in &language_of {
            let from = path_of[id];
            let mut direct: BTreeSet<i64> = BTreeSet::new();
            for spec in imports.get(id).into_iter().flatten() {
                for target in resolver.resolve(*lang, from, spec) {
                    if let Some(to) = id_of.get(target.as_str()) {
                        direct.insert(*to);
                        edges.insert((*id, *to, "import"), 1.0);
                    }
                }
            }
            // (b): a name used here and defined in exactly the files this one
            // imports — the first by path — is a reference to that file.
            for (name, kind) in refs.get(id).into_iter().flatten() {
                let Some(defs) = by_name.get(name) else {
                    continue;
                };
                let target = defs
                    .iter()
                    .filter(|d| *d != id && direct.contains(d))
                    .min_by_key(|d| path_of.get(d).copied().unwrap_or(""));
                if let Some(to) = target {
                    let label = if kind == "call" { "call" } else { "type" };
                    edges.entry((*id, *to, label)).or_insert(0.5);
                }
            }
        }
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM edges", [])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO edges (from_file, to_file, kind, weight) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for ((from, to, kind), weight) in &edges {
                insert.execute(params![from, to, kind, weight])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn snapshot(&self) -> Result<Arc<Snapshot>> {
        if let Some(cached) = lock(&self.snapshot).clone() {
            return Ok(cached);
        }
        let conn = lock(&self.conn);
        let mut paths: Vec<String> = Vec::new();
        let mut index_by_id: HashMap<i64, usize> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT id, path FROM files WHERE status = 'indexed' ORDER BY path")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (id, path) = row?;
                index_by_id.insert(id, paths.len());
                paths.push(path);
            }
        }
        let mut symbols: Vec<Vec<SymbolRow>> = vec![Vec::new(); paths.len()];
        {
            let mut stmt = conn.prepare(
                "SELECT file_id, name, simple_name, kind, line, end_line, container, signature, doc FROM symbols ORDER BY file_id, line, id",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    SymbolRow {
                        name: r.get(1)?,
                        simple_name: r.get(2)?,
                        kind: r.get(3)?,
                        line: r.get(4)?,
                        end_line: r.get(5)?,
                        container: r.get(6)?,
                        signature: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                        doc: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    },
                ))
            })? {
                let (file, symbol) = row?;
                if let Some(i) = index_by_id.get(&file) {
                    symbols[*i].push(symbol);
                }
            }
        }
        let mut edges = Vec::new();
        {
            let mut stmt = conn.prepare("SELECT from_file, to_file, weight FROM edges")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })? {
                let (from, to, weight) = row?;
                if let (Some(f), Some(t)) = (index_by_id.get(&from), index_by_id.get(&to)) {
                    edges.push(Edge {
                        from: *f,
                        to: *t,
                        weight,
                    });
                }
            }
        }
        let docs: Vec<Document> = paths
            .iter()
            .zip(&symbols)
            .map(|(path, syms)| Document {
                names: syms
                    .iter()
                    .map(|s| s.simple_name.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                path: path.clone(),
                signatures: syms
                    .iter()
                    .map(|s| s.signature.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                docs: syms
                    .iter()
                    .map(|s| s.doc.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            })
            .collect();
        let snapshot = Arc::new(Snapshot {
            corpus: Corpus::with(&docs, &lock(&self.params)),
            paths,
            edges,
            symbols,
        });
        *lock(&self.snapshot) = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// The files most worth showing for `query`, best first (§5.2).
    ///
    /// # Errors
    /// [`IndexError::Db`].
    pub fn rank(&self, query: &Query<'_>) -> Result<Vec<Ranked>> {
        let snap = self.snapshot()?;
        let n = snap.paths.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let by_path = rank::index_of(&snap.paths);
        let current = query.current_file.and_then(|p| by_path.get(p).copied());
        let touched: Vec<usize> = query
            .touched
            .iter()
            .filter_map(|p| by_path.get(p.as_str()).copied())
            .collect();
        let params = *lock(&self.params);
        let p = rank::personalization_with(n, current, &touched, &params);
        let pagerank = rank::pagerank_with(n, &snap.edges, &p, &params);
        let text = query.text.trim();
        let bm25 = if text.is_empty() {
            vec![0.0; n]
        } else {
            snap.corpus.score(text)
        };
        let scores = rank::blend_with(
            &pagerank,
            &bm25,
            text.is_empty(),
            n < rank::SPARSE_FILES,
            &params,
        );
        let terms: BTreeSet<String> = rank::tokens(text).into_iter().collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|a, b| {
            scores[*b]
                .partial_cmp(&scores[*a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| snap.paths[*a].cmp(&snap.paths[*b]))
        });
        let k = if query.top_k == 0 { TOP_K } else { query.top_k };
        Ok(order
            .into_iter()
            .take(k)
            .map(|i| {
                let symbols = snap.symbols[i].clone();
                let mut lines: Vec<u32> = symbols
                    .iter()
                    .filter(|s| {
                        rank::tokens(&s.simple_name)
                            .iter()
                            .any(|t| terms.contains(t))
                    })
                    .map(|s| s.line)
                    .collect();
                lines.dedup();
                Ranked {
                    path: snap.paths[i].clone(),
                    score: scores[i],
                    lines_of_interest: lines,
                    symbols,
                }
            })
            .collect())
    }
}

fn store_extracted(tx: &rusqlite::Transaction<'_>, file_id: i64, found: &Extracted) -> Result<()> {
    {
        let mut insert = tx.prepare(
            "INSERT INTO symbols (file_id, name, simple_name, kind, line, end_line, container, signature, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for s in &found.symbols {
            insert.execute(params![
                file_id,
                s.name,
                s.simple_name,
                s.kind,
                s.line,
                s.end_line,
                s.container,
                s.signature,
                s.doc
            ])?;
        }
    }
    {
        let mut insert = tx.prepare("INSERT INTO imports (file_id, spec) VALUES (?1, ?2)")?;
        for spec in &found.imports {
            insert.execute(params![file_id, spec])?;
        }
    }
    {
        let mut insert =
            tx.prepare("INSERT INTO refs (file_id, name, kind, n) VALUES (?1, ?2, ?3, ?4)")?;
        for (name, n) in &found.calls {
            insert.execute(params![file_id, name, "call", n])?;
        }
        for (name, n) in &found.type_refs {
            insert.execute(params![file_id, name, "type", n])?;
        }
    }
    Ok(())
}

/// A sortable timestamp without pulling in a date crate: seconds since the
/// epoch, as text.
fn chrono_like_now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or_else(|_| "0".to_string(), |d| d.as_secs().to_string())
}
