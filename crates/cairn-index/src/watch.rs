//! Keeping the index current while files change (SPEC §5.3, "Watcher").
//!
//! Events are collected for the debounce interval (300 ms) and handed over as
//! one batch of paths. A burst of more than 500 events in a second is a
//! storm — a branch switch, a build — and per-path updates would only fall
//! behind, so the watcher stops reporting paths and asks for a full rescan
//! every 60 seconds until the storm ends (`W-IDX-STORM`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cairn_search::IgnoreEngine;
use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// §5.3: how long a path must be quiet before it is reported.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// Events per second that make a storm.
pub const STORM_EVENTS_PER_SECOND: usize = 500;
/// In poll mode, how often everything is rescanned.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// What the watcher hands over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Batch {
    /// These paths changed (created, modified, removed or renamed).
    Paths(Vec<PathBuf>),
    /// A storm is on (or has just ended): rescan everything. Sent every
    /// [`POLL_INTERVAL`] while it lasts and once more when it ends.
    Rescan,
}

/// The debouncing and storm logic, with time passed in so it can be tested
/// without sleeping.
#[derive(Debug)]
pub struct Debouncer {
    pending: BTreeSet<PathBuf>,
    last_event: Option<Instant>,
    window_start: Option<Instant>,
    window_count: usize,
    storm: bool,
    last_poll: Option<Instant>,
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

impl Debouncer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: BTreeSet::new(),
            last_event: None,
            window_start: None,
            window_count: 0,
            storm: false,
            last_poll: None,
        }
    }

    /// Whether the watcher is in poll mode.
    #[must_use]
    pub const fn in_storm(&self) -> bool {
        self.storm
    }

    /// Treat the watcher as overwhelmed: events were lost, so only a rescan
    /// can be trusted.
    pub fn force_storm(&mut self, now: Instant) {
        if !self.storm {
            self.storm = true;
            self.last_poll = Some(now);
        }
        self.last_event = Some(now);
        self.pending.clear();
    }

    /// One event for `path` at `now`.
    pub fn event(&mut self, path: PathBuf, now: Instant) {
        match self.window_start {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {
                self.window_count += 1;
            }
            _ => {
                self.window_start = Some(now);
                self.window_count = 1;
            }
        }
        if self.window_count > STORM_EVENTS_PER_SECOND && !self.storm {
            self.storm = true;
            // The first rescan is a poll interval away, not immediate.
            self.last_poll = Some(now);
        }
        self.last_event = Some(now);
        if !self.storm {
            self.pending.insert(path);
        }
    }

    /// What is ready at `now`, if anything.
    pub fn poll(&mut self, now: Instant) -> Option<Batch> {
        if self.storm {
            let quiet = self
                .last_event
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
            let due = self
                .last_poll
                .is_none_or(|t| now.duration_since(t) >= POLL_INTERVAL);
            if quiet {
                // The storm is over: one last rescan catches what the
                // events skipped, then paths flow again.
                self.storm = false;
                self.pending.clear();
                self.last_poll = None;
                self.window_count = 0;
                return Some(Batch::Rescan);
            }
            if due {
                self.last_poll = Some(now);
                return Some(Batch::Rescan);
            }
            return None;
        }
        let quiet = self
            .last_event
            .is_some_and(|t| now.duration_since(t) >= DEBOUNCE);
        if quiet && !self.pending.is_empty() {
            self.last_event = None;
            let paths: Vec<PathBuf> = std::mem::take(&mut self.pending).into_iter().collect();
            return Some(Batch::Paths(paths));
        }
        None
    }
}

/// A running watcher. Dropping it stops the thread and the OS watch.
pub struct Watch {
    _watcher: RecommendedWatcher,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch").finish_non_exhaustive()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Events held between the OS callback and the debouncing thread.
const EVENT_QUEUE: usize = 65_536;

/// Whether an event kind changes what the index holds.
fn relevant(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(
                notify::event::ModifyKind::Data(_)
                    | notify::event::ModifyKind::Name(_)
                    | notify::event::ModifyKind::Any
            )
            | EventKind::Remove(_)
            | EventKind::Any
    )
}

/// Watch `root` recursively and call `on_batch` with each [`Batch`].
///
/// Paths the ignore rules exclude, anything under `.git`, and chmod-only
/// events are dropped before they count.
///
/// # Errors
/// The `notify` error when the watch cannot be set up.
pub fn watch(
    root: &Path,
    engine: &Arc<IgnoreEngine>,
    on_batch: impl Fn(Batch) + Send + 'static,
) -> Result<Watch, notify::Error> {
    // Bounded: a flood that fills it is a storm, and is treated as one.
    let (events_tx, events_rx) = sync_channel::<PathBuf>(EVENT_QUEUE);
    let overflowed = Arc::new(AtomicBool::new(false));
    let overflow_flag = Arc::clone(&overflowed);
    let root_buf = root.to_path_buf();
    let sender = Mutex::new(events_tx);
    let filter_engine = Arc::clone(engine);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return };
        if !relevant(event.kind) {
            return;
        }
        let tx = sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for path in event.paths {
            if filter_engine.is_ignored(&path, path.is_dir()) {
                continue;
            }
            if let Err(TrySendError::Full(_)) = tx.try_send(path) {
                overflow_flag.store(true, Ordering::Relaxed);
            }
        }
    })?;
    watcher.watch(&root_buf, RecursiveMode::Recursive)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let mut debouncer = Debouncer::new();
        loop {
            match events_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(path) => debouncer.event(path, Instant::now()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Drain what else is waiting before judging quiet.
            while let Ok(path) = events_rx.try_recv() {
                debouncer.event(path, Instant::now());
            }
            if stop_flag.load(Ordering::Relaxed) {
                return;
            }
            if overflowed.swap(false, Ordering::Relaxed) {
                debouncer.force_storm(Instant::now());
            }
            if let Some(batch) = debouncer.poll(Instant::now()) {
                on_batch(batch);
            }
        }
    });
    Ok(Watch {
        _watcher: watcher,
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn a_path_is_reported_once_it_has_been_quiet_for_the_debounce() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        d.event(p("a.rs"), t0);
        assert_eq!(d.poll(t0 + Duration::from_millis(299)), None);
        // A second event restarts the wait and joins the batch.
        d.event(p("b.rs"), t0 + Duration::from_millis(250));
        d.event(p("a.rs"), t0 + Duration::from_millis(260));
        assert_eq!(d.poll(t0 + Duration::from_millis(500)), None);
        assert_eq!(
            d.poll(t0 + Duration::from_millis(560)),
            Some(Batch::Paths(vec![p("a.rs"), p("b.rs")]))
        );
        assert_eq!(
            d.poll(t0 + Duration::from_secs(5)),
            None,
            "nothing is reported twice"
        );
    }

    #[test]
    fn more_than_500_events_in_a_second_is_a_storm() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        for i in 0..501 {
            d.event(
                p(&format!("f{i}")),
                t0 + Duration::from_millis(u64::try_from(i).unwrap()),
            );
        }
        assert!(d.in_storm());
        // No per-path batch while it lasts.
        assert_eq!(d.poll(t0 + Duration::from_millis(900)), None);
        // 500 events in a second is not yet a storm.
        let mut d = Debouncer::new();
        for i in 0..500 {
            d.event(
                p(&format!("f{i}")),
                t0 + Duration::from_millis(u64::try_from(i).unwrap()),
            );
        }
        assert!(!d.in_storm());
    }

    #[test]
    fn a_storm_ends_with_one_rescan_and_then_paths_flow_again() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        for i in 0..600 {
            d.event(
                p(&format!("f{i}")),
                t0 + Duration::from_millis(u64::try_from(i % 900).unwrap()),
            );
        }
        assert!(d.in_storm());
        // A second of quiet: the storm is over.
        assert_eq!(d.poll(t0 + Duration::from_secs(3)), Some(Batch::Rescan));
        assert!(!d.in_storm());
        d.event(p("later.rs"), t0 + Duration::from_secs(4));
        assert_eq!(
            d.poll(t0 + Duration::from_millis(4400)),
            Some(Batch::Paths(vec![p("later.rs")]))
        );
    }

    #[test]
    fn a_long_storm_rescans_every_sixty_seconds() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        for i in 0..600 {
            d.event(
                p("f"),
                t0 + Duration::from_millis(u64::try_from(i).unwrap()),
            );
        }
        // Events keep arriving, so it is never quiet; the poll fires on schedule.
        let mut rescans = 0;
        for second in 1..=130_u64 {
            let now = t0 + Duration::from_secs(second);
            d.event(p("still-busy"), now);
            if d.poll(now) == Some(Batch::Rescan) {
                rescans += 1;
            }
        }
        assert!((2..=3).contains(&rescans), "{rescans}");
    }

    #[test]
    fn lost_events_force_a_rescan() {
        let mut d = Debouncer::new();
        let t0 = Instant::now();
        d.event(p("a.rs"), t0);
        d.force_storm(t0 + Duration::from_millis(10));
        assert!(d.in_storm());
        assert_eq!(d.poll(t0 + Duration::from_millis(500)), None);
        assert_eq!(d.poll(t0 + Duration::from_secs(3)), Some(Batch::Rescan));
    }

    fn engine(root: &Path) -> Arc<IgnoreEngine> {
        Arc::new(IgnoreEngine::new(
            root,
            &cairn_search::IgnoreOptions::default(),
        ))
    }

    fn wait_for(batches: &Mutex<Vec<Batch>>, want: impl Fn(&[Batch]) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if want(&batches.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The real thing, against real file events.
    #[test]
    fn real_file_changes_arrive_as_one_debounced_batch_and_ignored_ones_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let batches: Arc<Mutex<Vec<Batch>>> = Arc::default();
        let sink = Arc::clone(&batches);
        let watcher = watch(&root, &engine(&root), move |b| sink.lock().unwrap().push(b)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn b() {}").unwrap();
        std::fs::write(root.join("ignored/x.rs"), "fn x() {}").unwrap();
        let reported = |all: &[Batch]| {
            all.iter().any(|b| matches!(b, Batch::Paths(p) if p.iter().any(|f| f.ends_with("src/a.rs")) && p.iter().any(|f| f.ends_with("src/b.rs"))))
        };
        assert!(
            wait_for(&batches, reported),
            "{:?}",
            batches.lock().unwrap()
        );
        let all = batches.lock().unwrap().clone();
        assert!(
            !all.iter().any(
                |b| matches!(b, Batch::Paths(p) if p.iter().any(|f| f.ends_with("ignored/x.rs")))
            ),
            "{all:?}"
        );
        drop(watcher);
    }

    #[test]
    fn removals_are_reported_and_stopping_is_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("gone.rs"), "fn g() {}").unwrap();
        let batches: Arc<Mutex<Vec<Batch>>> = Arc::default();
        let sink = Arc::clone(&batches);
        let watcher = watch(&root, &engine(&root), move |b| sink.lock().unwrap().push(b)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        let seen = |all: &[Batch]| {
            all.iter()
                .any(|b| matches!(b, Batch::Paths(p) if p.iter().any(|f| f.ends_with("gone.rs"))))
        };
        assert!(wait_for(&batches, seen));
        let started = Instant::now();
        drop(watcher);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
