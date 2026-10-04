//! Retention and cleanup (SPEC §11.7).
//!
//! Two rules, in this order:
//!
//! 1. **Never delete work in flight.** A session whose `plan_id` points at a
//!    plan that is still `in_progress` is skipped, however old it is.
//! 2. **Everything else ages out.** Sessions older than `session.retention_days`
//!    (90) and sessions beyond `session.max_sessions` (500) *per workspace* are
//!    retired oldest-first — retired meaning a `tombstone` record is appended
//!    first, and the file is unlinked only once `grace_days` (7) have passed.
//!
//! The two-phase delete is what makes `cairn sessions` honest in the window
//! between "retired" and "gone": a tombstoned session is hidden from listings
//! but its bytes are still recoverable.

use crate::record::Record;
use crate::store::{load_path, SessionFile, Store};
use chrono::{DateTime, Duration, Utc};
use std::path::{Path, PathBuf};

/// Knobs for one GC pass.
#[derive(Debug, Clone)]
pub struct Options {
    pub retention_days: u32,
    pub max_sessions: u32,
    /// Days between the tombstone and the unlink (SPEC §11.7).
    pub grace_days: u32,
    pub now: DateTime<Utc>,
    /// Restrict the pass to one workspace root.
    pub workspace: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            retention_days: 90,
            max_sessions: 500,
            grace_days: 7,
            now: Utc::now(),
            workspace: None,
        }
    }
}

/// What one GC pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Session files examined.
    pub scanned: usize,
    /// Sessions skipped because their plan is still `in_progress`.
    pub protected: usize,
    /// Session ids given a `tombstone` this pass.
    pub tombstoned: Vec<String>,
    /// Session ids whose file was unlinked this pass.
    pub unlinked: Vec<String>,
    pub bytes_freed: u64,
    /// Files that could not be processed, as `(path, message)`.
    pub errors: Vec<(PathBuf, String)>,
}

impl Report {
    /// True when the pass changed nothing (used by the `/sessions` footer).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.tombstoned.is_empty() && self.unlinked.is_empty() && self.errors.is_empty()
    }
}

/// Is this session's plan still open? The plan file is authoritative (§7.3);
/// the session header only carries the pointer.
#[must_use]
pub fn plan_in_progress(workspace: &Path, plan_id: &str) -> bool {
    let path = workspace
        .join(".cairn")
        .join("plans")
        .join(format!("{plan_id}.json"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|status| status == "in_progress")
}

/// The §11.7 protection rule: `header.plan_id` → plan file → status.
#[must_use]
pub fn is_protected(header: &crate::record::Header) -> bool {
    header
        .plan_id
        .as_deref()
        .is_some_and(|plan_id| plan_in_progress(Path::new(&header.workspace), plan_id))
}

/// Run one pass. Never aborts on a single bad file: failures are collected in
/// [`Report::errors`] so a maintenance run reports everything it saw.
pub fn gc(
    store: &Store,
    opts: &Options,
    protected: impl Fn(&crate::record::Header) -> bool,
) -> Report {
    let mut report = Report::default();
    let mut by_workspace: Vec<Vec<SessionFile>> = Vec::new();

    for path in store.paths() {
        report.scanned += 1;
        match load_path(&path) {
            Ok(file) => {
                if let Some(ws) = &opts.workspace {
                    if Path::new(&file.header.workspace) != ws.as_path() {
                        report.scanned -= 1;
                        continue;
                    }
                }
                let same = |g: &Vec<SessionFile>| {
                    g.first()
                        .is_some_and(|f| f.header.workspace == file.header.workspace)
                };
                match by_workspace.iter().position(same) {
                    Some(index) => by_workspace[index].push(file),
                    None => by_workspace.push(vec![file]),
                }
            }
            Err(e) => report.errors.push((path, e.message)),
        }
    }

    for group in by_workspace {
        run_group(group, opts, &protected, &mut report);
    }
    report
}

fn run_group(
    group: Vec<SessionFile>,
    opts: &Options,
    protected: &impl Fn(&crate::record::Header) -> bool,
    report: &mut Report,
) {
    // Phase 2 first: files already tombstoned are unlinked once the grace
    // period is up, which also stops them counting against `max_sessions`.
    let mut live = Vec::new();
    for file in group {
        match file.tombstoned_at() {
            Some(deleted_at) => {
                let age = parse_ts(deleted_at).map(|at| opts.now - at);
                if age.is_some_and(|d| d >= Duration::days(i64::from(opts.grace_days))) {
                    unlink(&file, report);
                }
            }
            None => live.push(file),
        }
    }

    // Oldest first: the sessions that lose the competition for a slot are the
    // ones at the front of this list.
    live.sort_by(|a, b| a.header.created_at.cmp(&b.header.created_at));
    let cutoff = opts.now - Duration::days(i64::from(opts.retention_days));

    for (index, file) in live.iter().enumerate() {
        let over_count = index as u64 >= u64::from(opts.max_sessions);
        let over_age = parse_ts(&file.header.created_at).is_some_and(|created| created < cutoff);
        if !over_count && !over_age {
            continue;
        }
        if protected(&file.header) {
            report.protected += 1;
            continue;
        }
        let reason = if over_age {
            "retention"
        } else {
            "max_sessions"
        };
        let mut edited = file.clone();
        edited.push(Record::tombstone(reason));
        match store_save(&edited) {
            Ok(()) => report.tombstoned.push(edited.header.session_id.clone()),
            Err(e) => report.errors.push((edited.path.clone(), e.message)),
        }
    }
}

fn store_save(file: &SessionFile) -> crate::Result<()> {
    crate::store::atomic_write(&file.path, &file.to_bytes())
}

fn unlink(file: &SessionFile, report: &mut Report) {
    let bytes = std::fs::metadata(&file.path).map_or(0, |m| m.len());
    match std::fs::remove_file(&file.path) {
        Ok(()) => {
            report.bytes_freed += bytes;
            report.unlinked.push(file.header.session_id.clone());
        }
        Err(e) => report.errors.push((file.path.clone(), e.to_string())),
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Should this process run GC now? Throttled to once per 24 h (SPEC §11.7),
/// where `last` is the mtime of the marker file the caller keeps.
#[must_use]
pub fn due(last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last.is_none_or(|at| now - at >= Duration::hours(24))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Header;

    fn setup(tmp: &tempfile::TempDir) -> (Store, PathBuf) {
        let store = Store::new(tmp.path().join("sessions"));
        (store, tmp.path().join("ws"))
    }

    fn add(store: &Store, ws: &Path, id: &str, created_at: &str, plan_id: Option<&str>) {
        let mut header = Header::new(id, ws.to_string_lossy(), "build", "m");
        header.created_at = created_at.to_string();
        header.plan_id = plan_id.map(str::to_string);
        store.create(&header).unwrap();
    }

    fn at(days_ago: i64) -> String {
        (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// T-SESS-030: 600 sessions, 90-day retention, `in_progress` protected.
    #[test]
    fn gc_retires_the_oldest_and_protects_live_plans() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, ws) = setup(&tmp);

        for i in 0..600 {
            let id = format!("ses_{i:03}");
            let created = if i < 30 { at(100) } else { at(1) };
            add(&store, &ws, &id, &created, None);
        }
        // Oldest of all, but its plan is still open.
        add(
            &store,
            &ws,
            "ses_live",
            &at(200),
            Some("2026-01-01T00-00-00Z-still-going"),
        );

        let plans = ws.join(".cairn/plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(
            plans.join("2026-01-01T00-00-00Z-still-going.json"),
            r#"{"schema_version":1,"plan_id":"2026-01-01T00-00-00Z-still-going","status":"in_progress"}"#,
        )
        .unwrap();

        let opts = Options {
            retention_days: 90,
            max_sessions: 500,
            grace_days: 7,
            now: Utc::now(),
            workspace: None,
        };
        let report = gc(&store, &opts, is_protected);

        assert_eq!(report.scanned, 601);
        assert_eq!(report.protected, 1, "the in_progress plan is never deleted");
        // 30 over retention + the 101 that do not fit under max_sessions.
        assert_eq!(report.tombstoned.len(), 131, "{}", report.tombstoned.len());
        assert!(report.tombstoned.contains(&"ses_000".to_string()));
        assert!(
            !report.tombstoned.contains(&"ses_live".to_string()),
            "protected session must not be tombstoned"
        );
        assert!(
            report.unlinked.is_empty(),
            "nothing unlinks before the grace period"
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        // Tombstoned sessions are gone from listings but still on disk.
        assert_eq!(
            store.list(&crate::store::ListFilter::default()).len(),
            470,
            "601 minus the 131 retired"
        );
        assert!(store.find("ses_000").is_some());

        // A second pass, a week and a day later, unlinks them.
        let later = Options {
            now: Utc::now() + Duration::days(8),
            ..opts.clone()
        };
        let second = gc(&store, &later, is_protected);
        assert_eq!(second.unlinked.len(), 131, "{}", second.unlinked.len());
        assert!(second.bytes_freed > 0);
        assert!(store.find("ses_000").is_none());
        assert_eq!(
            store.list(&crate::store::ListFilter::default()).len(),
            470,
            "the protected session survives both passes"
        );
    }

    #[test]
    fn a_corrupt_file_is_reported_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, ws) = setup(&tmp);
        add(&store, &ws, "ses_ok", &at(1), None);
        let bad = store.dir_for(&ws).join("ses_bad.jsonl");
        std::fs::write(&bad, "not json\n").unwrap();

        let report = gc(&store, &Options::default(), is_protected);
        assert_eq!(report.scanned, 2);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].0, bad);
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "not json\n");
    }

    #[test]
    fn a_workspace_filter_keeps_other_workspaces_out() {
        let tmp = tempfile::tempdir().unwrap();
        let (store, ws) = setup(&tmp);
        let other = tmp.path().join("other");
        add(&store, &ws, "ses_here", &at(200), None);
        add(&store, &other, "ses_there", &at(200), None);

        let report = gc(
            &store,
            &Options {
                workspace: Some(ws.clone()),
                ..Options::default()
            },
            |_| false,
        );
        assert_eq!(report.tombstoned, vec!["ses_here".to_string()]);
        assert!(
            store.find("ses_there").is_some(),
            "the other workspace is untouched"
        );
        assert!(
            store.find("ses_here").is_some(),
            "tombstoned, not yet unlinked"
        );
    }

    #[test]
    fn gc_runs_at_most_once_a_day() {
        let now = Utc::now();
        assert!(due(None, now));
        assert!(!due(Some(now - Duration::hours(23)), now));
        assert!(due(Some(now - Duration::hours(25)), now));
    }

    #[test]
    fn a_plan_that_is_not_in_progress_does_not_protect_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let plans = ws.join(".cairn/plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(
            plans.join("p.json"),
            r#"{"schema_version":1,"plan_id":"p","status":"completed"}"#,
        )
        .unwrap();
        assert!(!plan_in_progress(&ws, "p"));
        assert!(!plan_in_progress(&ws, "missing"));

        let (store, _) = setup(&tmp);
        add(&store, &ws, "ses_done", &at(200), Some("p"));
        let report = gc(&store, &Options::default(), is_protected);
        assert_eq!(report.protected, 0);
        assert_eq!(report.tombstoned, vec!["ses_done".to_string()]);
    }
}
