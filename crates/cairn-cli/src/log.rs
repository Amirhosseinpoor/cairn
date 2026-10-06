//! §12.1 structured logging: JSONL file records with size rotation,
//! redaction, levels, and a human stderr layer above the default verbosity.
//!
//! Three pieces, each boring on purpose:
//!
//! * [`RotatingFile`] — an append-only file with size rotation (`rotate_bytes`,
//!   keep N) and a total-bytes cap over the log dir. `tracing-appender` only
//!   rotates daily, so this is hand-rolled (§15.2 records why it is absent).
//! * [`NonBlocking`] — a bounded channel plus a worker thread: records never
//!   block the caller, overflow drops with a counter (REQ-OPS-001), and the
//!   file is flushed every second, after every `error`, and on shutdown.
//! * [`JsonlLayer`] — a `tracing` layer emitting §12.1's record shape
//!   (`ts`, `level`, `target`, `session`, `turn_id`, `event`, `code`, `msg`,
//!   `kv`), with every line through the [`Redactor`] before it is written.
//!
//! [`init`] wires all three from config: the file layer at the configured
//! level, plus a colored human stderr layer when the invocation asked for
//! more than the default verbosity. The returned [`LogGuard`] must be shut
//! down — dropping it only stops the worker, joining flushes the last
//! records to disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use cairn_config::{Config, LogLevel};
use cairn_core::error::{codes, ExitStatus};
use cairn_core::redact::Redactor;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

use crate::output::Fail;

/// §12.1 caps the whole log directory at 50 MiB, hard-coded like the other
/// retention numbers in that section.
const TOTAL_CAP_BYTES: u64 = 50 * 1024 * 1024;

/// Bound of the non-blocking queue: past it, records drop and
/// [`LogGuard::dropped`] counts them (REQ-OPS-001).
const QUEUE_DEPTH: usize = 1024;

/// How long [`LogGuard::shutdown`] keeps trying to queue `Stop` (50 x 10 ms).
const STOP_ATTEMPTS: u32 = 50;
const STOP_RETRY: Duration = Duration::from_millis(10);

/// How often the worker flushes without being asked.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// What the worker thread receives.
enum Item {
    Line(Vec<u8>),
    /// Flush now — the layer sends one after every `error` record (§12.1).
    Flush,
    /// Flush and exit. The global subscriber keeps its layer, and so a
    /// sender, alive for the whole process, so the channel never disconnects
    /// on its own and shutdown cannot wait for that.
    Stop,
}

/// An append-only log file with size rotation and a directory cap.
///
/// Rotation renames `cairn.log` → `cairn.log.1` → … keeping `keep` files,
/// then prunes oldest-first until the directory is back under the cap — the
/// current file is never pruned. Lines are written whole: rotation happens
/// *before* a line that would overflow, so a record is never split across
/// files. New files are `0600` on Unix; the content is redacted, but
/// defense in depth is cheaper than an incident.
struct RotatingFile {
    path: PathBuf,
    rotate_bytes: u64,
    keep: u32,
    total_cap: u64,
    file: Option<File>,
    written: u64,
}

impl RotatingFile {
    fn open(path: &Path, append: bool) -> io::Result<File> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).write(true).append(append);
        if !append {
            options.truncate(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }

    fn new(path: PathBuf, rotate_bytes: u64, keep: u32, total_cap: u64) -> io::Result<Self> {
        let written = fs::metadata(&path).map_or(0, |meta| meta.len());
        let file = Self::open(&path, true)?;
        Ok(Self {
            path,
            rotate_bytes,
            keep,
            total_cap,
            file: Some(file),
            written,
        })
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.written + line.len() as u64 + 1 > self.rotate_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().expect("file is open");
        file.write_all(line)?;
        file.write_all(b"\n")?;
        self.written += line.len() as u64 + 1;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }

    fn sibling(&self, generation: u32) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{generation}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Close first: Windows will not rename an open file.
        drop(self.file.take());
        if self.keep > 0 {
            let _ = fs::remove_file(self.sibling(self.keep));
            for generation in (1..self.keep).rev() {
                let _ = fs::rename(self.sibling(generation), self.sibling(generation + 1));
            }
            let _ = fs::rename(&self.path, self.sibling(1));
        }
        self.file = Some(Self::open(&self.path, false)?);
        self.written = 0;
        self.prune()
    }

    /// Delete oldest-first until the directory is back under the cap. Age
    /// comes from mtime with a generation-number fallback; the current file is exempt.
    fn prune(&self) -> io::Result<()> {
        let dir = self.path.parent();
        let Some(dir) = dir else {
            return Ok(());
        };
        let stem = self
            .path
            .file_name()
            .map(std::borrow::ToOwned::to_owned)
            .unwrap_or_default();
        let stem = stem.to_string_lossy().into_owned();
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with(stem.as_str()) {
                continue;
            }
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            entries.push((
                entry.path(),
                meta.len(),
                meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            ));
        }
        // Equal mtimes (a coarse clock) fall back to the generation suffix:
        // `.3` is older than `.1`, and the current file is generation 0.
        let generation = |path: &Path| {
            path.to_string_lossy()
                .rsplit('.')
                .next()
                .and_then(|suffix| suffix.parse::<u32>().ok())
                .unwrap_or(0)
        };
        entries.sort_by(|a, b| {
            a.2.cmp(&b.2)
                .then_with(|| generation(&b.0).cmp(&generation(&a.0)))
        });
        for (path, size, _) in entries {
            if total <= self.total_cap {
                break;
            }
            if path == self.path {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total -= size;
            }
        }
        Ok(())
    }
}

/// The worker half of [`NonBlocking`]: drains the channel into the file,
/// flushing every second of quiet, on every [`Item::Flush`], and once more
/// when the last sender goes away.
/// The receiver is passed by value so it's owned by this thread for its
/// lifetime, which is necessary for the spawn contract to work. clippy's
/// `needless_pass_by_value` does not account for ownership transfer across
/// thread boundaries.
#[allow(clippy::needless_pass_by_value)]
fn run_worker(receiver: mpsc::Receiver<Item>, mut file: RotatingFile) {
    loop {
        match receiver.recv_timeout(FLUSH_INTERVAL) {
            Ok(Item::Line(line)) => {
                let _ = file.write_line(&line);
            }
            Ok(Item::Flush) | Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = file.flush();
            }
            Ok(Item::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = file.flush();
                break;
            }
        }
    }
}

/// A file sink that never blocks its caller: records queue bounded, overflow
/// drops with a count, and a worker thread owns the file.
struct NonBlocking {
    sender: mpsc::SyncSender<Item>,
    dropped: Arc<AtomicU64>,
}

impl NonBlocking {
    fn new(file: RotatingFile) -> (Self, std::thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_DEPTH);
        let worker = std::thread::spawn(move || run_worker(receiver, file));
        (
            Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            worker,
        )
    }

    fn enqueue(&self, item: Item) {
        if self.sender.try_send(item).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `tracing` fields collected per event. The §12.1 fixed keys are hoisted to
/// the top level; everything else nests under `kv`.
#[derive(Default)]
struct Fields {
    map: serde_json::Map<String, serde_json::Value>,
}

impl Fields {
    fn insert(&mut self, key: &str, value: serde_json::Value) {
        self.map.insert(key.to_string(), value);
    }
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.insert(field.name(), serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field.name(), serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field.name(), serde_json::Value::from(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field.name(), serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.insert(field.name(), serde_json::Value::from(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.insert(field.name(), serde_json::Value::from(format!("{value:?}")));
    }
}

/// The file half of §12.1: one JSONL record per event, redacted, queued.
struct JsonlLayer {
    redactor: Option<Redactor>,
    sink: NonBlocking,
}

impl<S> Layer<S> for JsonlLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut fields = Fields::default();
        event.record(&mut fields);

        let mut record = serde_json::Map::new();
        record.insert(
            "ts".to_string(),
            serde_json::Value::from(
                chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string(),
            ),
        );
        record.insert(
            "level".to_string(),
            serde_json::Value::from(meta.level().as_str().to_lowercase()),
        );
        record.insert("target".to_string(), serde_json::Value::from(meta.target()));
        for key in ["session", "turn_id", "event", "code"] {
            if let Some(value) = fields.map.remove(key) {
                record.insert(key.to_string(), value);
            }
        }
        let message = fields
            .map
            .remove("message")
            .unwrap_or(serde_json::Value::Null);
        record.insert("msg".to_string(), message);
        record.insert("kv".to_string(), serde_json::Value::Object(fields.map));

        let mut line = serde_json::to_string(&record).expect("a JSON map serialises");
        if let Some(redactor) = &self.redactor {
            line = redactor.redact(&line);
        }
        self.sink.enqueue(Item::Line(line.into_bytes()));
        // `tracing` orders by verbosity (TRACE is the greatest), so `>= ERROR`
        // would match every level.
        if *meta.level() == tracing::Level::ERROR {
            self.sink.enqueue(Item::Flush);
        }
    }
}

/// The [`Redactor`] every file record passes through: §9.6's static
/// patterns, the user's `security.redact_patterns`, the configured `api_key`
/// of every provider, and every `*_API_KEY` in the process environment —
/// which covers the §4.10 env steps without naming providers here. Invalid
/// user patterns are skipped: a broken regex must not break logging (config
/// validation owns that complaint).
///
/// `None` only when `log.redact = false`, which startup already gated behind
/// `trace.debug_unsafe` (`E-CFG-UNSAFEREDACT`).
#[must_use]
pub fn redactor_for(config: &Config) -> Option<Redactor> {
    if !config.log.redact {
        return None;
    }
    let mut redactor = Redactor::default();
    for pattern in &config.security.redact_patterns {
        let _ = redactor.add_pattern(pattern);
    }
    for provider in config.providers.values() {
        if !provider.api_key.is_empty() {
            redactor.add_secret_value(&provider.api_key);
        }
    }
    for (key, value) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if key.ends_with("API_KEY") {
            if let Some(value) = value.to_str() {
                redactor.add_secret_value(value);
            }
        }
    }
    Some(redactor)
}

fn file_filter(level: LogLevel) -> tracing_subscriber::filter::LevelFilter {
    use tracing_subscriber::filter::LevelFilter;
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Parse a `--log-level` spelling. `None` (flag absent) and unparseable
/// values both fall back to the configured level — the flag layer is
/// validated at startup, so this is only load-bearing for tests.
fn parse_level(text: Option<&str>) -> Option<tracing::Level> {
    let text = text?;
    match text.to_lowercase().as_str() {
        "error" => Some(tracing::Level::ERROR),
        "warn" | "warning" => Some(tracing::Level::WARN),
        "info" => Some(tracing::Level::INFO),
        "debug" => Some(tracing::Level::DEBUG),
        "trace" => Some(tracing::Level::TRACE),
        _ => None,
    }
}

/// Resolve the log file: `--log-file`, then `log.file` (`~`-expanded), then
/// the §11.6 default.
fn log_path(config: &Config, log_file: Option<&Path>, default: &Path) -> PathBuf {
    if let Some(path) = log_file {
        return path.to_path_buf();
    }
    if !config.log.file.trim().is_empty() {
        let get = |key: &str| std::env::var(key).ok();
        return cairn_config::expand_tilde(config.log.file.trim(), &get);
    }
    default.to_path_buf()
}

/// The logging session [`init`] installs. Hold it for the process lifetime
/// and [`LogGuard::shutdown`] it on the way out — dropping alone stops the
/// worker, joining flushes the last records to disk.
pub struct LogGuard {
    sender: Option<mpsc::SyncSender<Item>>,
    worker: Option<std::thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl LogGuard {
    /// Records dropped to overflow so far (REQ-OPS-001's `log.dropped`).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Flush, join the worker, and report what was lost. Idempotent only in
    /// the sense that calling it twice joins nothing the second time.
    #[must_use]
    pub fn shutdown(mut self) -> u64 {
        if let Some(sender) = self.sender.take() {
            // A full queue drains within a few milliseconds; if it somehow
            // does not, leave the worker unjoined rather than hang the exit.
            let mut stopped = false;
            for _ in 0..STOP_ATTEMPTS {
                if sender.try_send(Item::Stop).is_ok() {
                    stopped = true;
                    break;
                }
                std::thread::sleep(STOP_RETRY);
            }
            if stopped {
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
        }
        self.dropped()
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        // Stop the worker without joining: on a panic path a join could hang
        // shutdown, and the process is exiting anyway.
        self.sender.take();
    }
}

/// Install §12.1 logging for this invocation: the redacted JSONL file at the
/// configured level, plus a colored human stderr layer when the invocation
/// asked for more than the default verbosity (`--log-level`, `-v`).
pub fn init(
    config: &Config,
    default_log_file: &Path,
    log_level: Option<&str>,
    verbose: u8,
    log_file: Option<&Path>,
) -> Result<LogGuard, Fail> {
    use std::io::IsTerminal;

    let path = log_path(config, log_file, default_log_file);
    let file = RotatingFile::new(
        path,
        config.log.rotate_bytes,
        config.log.keep_rotated,
        TOTAL_CAP_BYTES,
    )
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => Fail::new(
            codes::FS_PERM,
            ExitStatus::Permission,
            format!("log file is not writable: {error}"),
            Some("check the directory's permissions".to_string()),
        ),
        _ => Fail::new(
            "ERR_GENERIC",
            ExitStatus::Generic,
            format!("cannot open the log file: {error}"),
            None,
        ),
    })?;
    let (sink, worker) = NonBlocking::new(file);
    let sender = sink.sender.clone();
    let dropped = Arc::clone(&sink.dropped);
    let layer = JsonlLayer {
        redactor: redactor_for(config),
        sink,
    };
    let file_level = parse_level(log_level).map_or_else(
        || config.log.level,
        |level| match level {
            tracing::Level::ERROR => LogLevel::Error,
            tracing::Level::WARN => LogLevel::Warn,
            tracing::Level::INFO => LogLevel::Info,
            tracing::Level::DEBUG => LogLevel::Debug,
            tracing::Level::TRACE => LogLevel::Trace,
        },
    );
    let subscriber =
        tracing_subscriber::registry().with(layer.with_filter(file_filter(file_level)));

    // Louder than the default gets a human layer on stderr (§12.1); the
    // file always records at its own level regardless.
    let verbose_level = match verbose {
        0 => None,
        1 => Some(tracing::Level::INFO),
        2 => Some(tracing::Level::DEBUG),
        _ => Some(tracing::Level::TRACE),
    };
    let loudest = parse_level(log_level)
        .into_iter()
        .chain(verbose_level)
        .max();
    if loudest.is_some_and(|level| level > tracing::Level::WARN) {
        let stderr = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal() && !no_color_env())
            .with_filter(loudest.map_or(
                tracing_subscriber::filter::LevelFilter::INFO,
                file_filter_level,
            ));
        tracing::subscriber::set_global_default(subscriber.with(stderr)).map_err(|_| {
            Fail::new(
                "ERR_GENERIC",
                ExitStatus::Generic,
                "a logging subscriber is already installed",
                None,
            )
        })?;
    } else {
        tracing::subscriber::set_global_default(subscriber).map_err(|_| {
            Fail::new(
                "ERR_GENERIC",
                ExitStatus::Generic,
                "a logging subscriber is already installed",
                None,
            )
        })?;
    }
    Ok(LogGuard {
        sender: Some(sender),
        worker: Some(worker),
        dropped,
    })
}

fn no_color_env() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

fn file_filter_level(level: tracing::Level) -> tracing_subscriber::filter::LevelFilter {
    use tracing_subscriber::filter::LevelFilter;
    match level {
        tracing::Level::ERROR => LevelFilter::ERROR,
        tracing::Level::WARN => LevelFilter::WARN,
        tracing::Level::INFO => LevelFilter::INFO,
        tracing::Level::DEBUG => LevelFilter::DEBUG,
        tracing::Level::TRACE => LevelFilter::TRACE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cairn-log-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join("cairn.log")
    }

    fn read_lines(path: &Path) -> Vec<String> {
        let mut lines = Vec::new();
        let dir = path.parent().expect("parent");
        let stem = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(stem.as_str()))
            })
            .collect();
        // Oldest first: the highest generation suffix, the current file last.
        let generation = |path: &Path| {
            path.extension()
                .and_then(|ext| ext.to_string_lossy().parse::<u32>().ok())
                .unwrap_or(0)
        };
        files.sort_by_key(|path| std::cmp::Reverse(generation(path)));
        for file in files {
            let text = fs::read_to_string(&file).expect("read");
            lines.extend(text.lines().map(str::to_string));
        }
        lines
    }

    /// Rotation keeps every line whole: the file that would overflow closes
    /// first, and the generations chain off it.
    #[test]
    fn rotation_never_splits_a_line() {
        let path = temp_path("rotate");
        let mut file = RotatingFile::new(path.clone(), 200, 2, TOTAL_CAP_BYTES).expect("open");
        for n in 0..10 {
            file.write_line(format!("line {n:02} with padding to pass fifty bytes....").as_bytes())
                .expect("write");
        }
        drop(file);
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 10);
        for (n, line) in lines.iter().enumerate() {
            assert!(line.starts_with(&format!("line {n:02}")), "{line}");
        }
        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    /// The directory cap prunes oldest-first and never touches the current
    /// file, however far over budget the directory starts.
    #[test]
    fn the_directory_cap_prunes_oldest_first() {
        let path = temp_path("prune");
        let dir = path.parent().expect("parent").to_path_buf();
        // Oldest first, as rotation would have left them.
        for generation in (1..=3).rev() {
            let stale = dir.join(format!("cairn.log.{generation}"));
            fs::write(&stale, vec![b'x'; 100]).expect("write");
        }
        let mut file = RotatingFile::new(path.clone(), 10_485_760, 3, 250).expect("open");
        file.write_line(b"current").expect("write");
        // 3 stale x 100 bytes + the current line is over the 250 cap by 58:
        // exactly one file has to go, and it is the oldest generation.
        file.prune().expect("prune");
        assert!(
            !dir.join("cairn.log.3").exists(),
            "the oldest generation goes first"
        );
        assert!(dir.join("cairn.log.2").exists());
        assert!(dir.join("cairn.log.1").exists());
        assert!(path.exists(), "the current file is never pruned");

        // Far over budget: every stale generation goes, the current file stays.
        file.total_cap = 1;
        file.prune().expect("prune");
        assert!(!dir.join("cairn.log.1").exists());
        assert!(!dir.join("cairn.log.2").exists());
        assert!(path.exists(), "the current file is never pruned");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A coarse clock gives every file the same mtime; the generation suffix
    /// then decides, and `.3` is older than `.1`.
    #[test]
    fn equal_mtimes_prune_the_highest_generation_first() {
        let path = temp_path("ties");
        let dir = path.parent().expect("parent").to_path_buf();
        let stamp = std::time::SystemTime::now();
        for generation in 1..=3 {
            let stale = dir.join(format!("cairn.log.{generation}"));
            fs::write(&stale, vec![b'x'; 100]).expect("write");
            File::options()
                .write(true)
                .open(&stale)
                .and_then(|f| f.set_modified(stamp))
                .expect("set mtime");
        }
        let file = RotatingFile::new(path.clone(), 10_485_760, 3, 250).expect("open");
        file.prune().expect("prune");
        assert!(!dir.join("cairn.log.3").exists());
        assert!(dir.join("cairn.log.1").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The global subscriber keeps its layer, and so a sender, alive for the
    /// life of the process: shutdown must flush and return anyway.
    #[test]
    fn shutdown_returns_while_a_layer_still_holds_a_sender() {
        let path = temp_path("shutdown");
        let file = RotatingFile::new(path.clone(), 10_485_760, 3, TOTAL_CAP_BYTES).expect("open");
        let (sink, worker) = NonBlocking::new(file);
        let guard = LogGuard {
            sender: Some(sink.sender.clone()),
            worker: Some(worker),
            dropped: Arc::clone(&sink.dropped),
        };
        sink.enqueue(Item::Line(b"last words".to_vec()));
        let (done, finished) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = done.try_send(guard.shutdown());
        });
        let dropped = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown must not wait for the layer's sender to go away");
        assert_eq!(dropped, 0);
        assert_eq!(read_lines(&path), vec!["last words".to_string()]);
        drop(sink);
        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    /// A scoped subscriber through the real layer writes one redacted JSONL
    /// record per event, with §12.1's fixed keys on top.
    #[test]
    fn the_layer_writes_redacted_jsonl() {
        let path = temp_path("jsonl");
        let file = RotatingFile::new(path.clone(), 10_485_760, 3, TOTAL_CAP_BYTES).expect("open");
        let (sink, worker) = NonBlocking::new(file);
        let mut redactor = Redactor::default();
        redactor.add_secret_value("hunter2-secret");
        let layer = JsonlLayer {
            redactor: Some(redactor),
            sink,
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);
        tracing::warn!(
            event = "stream.lossy_utf8",
            code = "W-PROV-LOSSY",
            count = 3u64,
            "substituted hunter2-secret bytes"
        );
        drop(guard);
        // The dispatch owns the layer, hence the sender; the worker only
        // drains once the last sender is gone.
        drop(dispatch);
        worker.join().expect("worker drains");

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(&lines[0]).expect("JSONL");
        assert_eq!(record["level"], "warn");
        assert_eq!(record["event"], "stream.lossy_utf8");
        assert_eq!(record["code"], "W-PROV-LOSSY");
        assert_eq!(record["kv"]["count"], 3);
        let rendered = lines[0].clone();
        assert!(!rendered.contains("hunter2-secret"), "redacted: {rendered}");
        assert!(rendered.contains("***REDACTED***"), "{rendered}");
        std::fs::remove_dir_all(path.parent().expect("parent")).ok();
    }

    /// `redactor_for` wires §9.6's statics, the user's
    /// `security.redact_patterns`, configured keys, and the process env.
    #[test]
    fn redactor_for_wires_patterns_keys_and_env() {
        let mut config = Config::default();
        config
            .security
            .redact_patterns
            .push("(?i)corp-secret-[0-9]+".to_string());
        let redactor = redactor_for(&config).expect("redacting");
        assert!(
            redactor.pattern_count() > Redactor::default().pattern_count(),
            "the user pattern landed"
        );
        assert_eq!(
            redactor.redact("leak corp-secret-123 here"),
            "leak ***REDACTED*** here"
        );
    }

    /// `log.redact = false` means no redactor at all — startup gates the
    /// combination, this only honours it.
    #[test]
    fn no_redact_flag_means_no_redactor() {
        let mut config = Config::default();
        config.log.redact = false;
        assert!(redactor_for(&config).is_none());
    }

    /// Level spellings resolve, and anything else falls back to the
    /// configured level instead of failing the invocation.
    #[test]
    fn level_spellings_resolve() {
        assert_eq!(parse_level(Some("debug")), Some(tracing::Level::DEBUG));
        assert_eq!(parse_level(Some("WARNING")), Some(tracing::Level::WARN));
        assert_eq!(parse_level(None), None);
        assert_eq!(parse_level(Some("verbose")), None);
    }
}
