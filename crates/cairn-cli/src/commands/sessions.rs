//! `cairn sessions` (SPEC §11.1, §11.7) — a read-only view over the real JSONL
//! session store: every workspace directory, header records only, retired
//! sessions hidden (a `tombstone` is not a session you can go back to).

use crate::args::SessionsArgs;
use crate::commands::Startup;
use crate::output::Fail;
use cairn_session::{Header, ListFilter, Store, Summary};
use std::path::PathBuf;

/// Header metadata of one session file (SPEC §11.7 `header` record).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SessionMeta {
    pub session_id: String,
    pub created_at: String,
    pub workspace: String,
    pub mode: String,
    pub model: String,
    pub cairn_version: String,
    pub path: PathBuf,
}

/// The session store for this invocation (SPEC §11.6 sessions dir).
#[must_use]
pub fn store(startup: &Startup) -> Store {
    Store::new(startup.loaded.paths.sessions_dir())
}

fn meta(summary: Summary) -> SessionMeta {
    let h: Header = summary.header;
    SessionMeta {
        session_id: h.session_id,
        created_at: h.created_at,
        workspace: h.workspace,
        mode: h.mode,
        model: h.model,
        cairn_version: h.cairn_version,
        path: summary.path,
    }
}

/// Every stored session, newest first (string-ordered RFC3339 timestamps).
#[cfg(test)]
pub fn scan(startup: &Startup) -> Vec<SessionMeta> {
    store(startup)
        .list_all(&ListFilter::default())
        .into_iter()
        .map(meta)
        .collect()
}

/// The header of one session file, or `None` if it is not a loadable session.
///
/// Listing is tolerant by design: a file that exists but cannot be read is
/// skipped, and `E-SESS-CORRUPT` is reported where the file is actually
/// needed — `export` and `resume` (SPEC §11.7 read contract).
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
fn read_header(path: &Path) -> Option<SessionMeta> {
    let file = cairn_session::load_path(path).ok()?;
    Some(meta(Summary {
        path: file.path,
        header: file.header,
    }))
}

/// `cairn sessions [--json] [--limit N] [--workspace PATH] [--grep TEXT]`.
pub fn list(args: &SessionsArgs, startup: &Startup) -> Result<i32, Fail> {
    let rows = filtered(args, startup);
    if args.json {
        say!(
            "{}",
            serde_json::to_string_pretty(&rows).expect("sessions serialize")
        );
        return Ok(0);
    }
    if rows.is_empty() {
        if !startup.quiet {
            say!("no sessions");
        }
        return Ok(0);
    }
    for m in &rows {
        say!(
            "{}  {}  {}  {}  {}",
            m.session_id,
            m.created_at,
            m.mode,
            m.model,
            m.workspace
        );
    }
    if !startup.quiet {
        say!("{} session(s)", rows.len());
    }
    Ok(0)
}

fn filtered(args: &SessionsArgs, startup: &Startup) -> Vec<SessionMeta> {
    let workspace = args
        .workspace
        .as_ref()
        .map(|w| std::fs::canonicalize(w).unwrap_or_else(|_| w.clone()));
    store(startup)
        .list_all(&ListFilter {
            workspace,
            grep: args.grep.clone(),
            limit: Some(args.limit.max(1)),
            include_deleted: false,
        })
        .into_iter()
        .map(meta)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use std::collections::BTreeMap;

    /// A `Startup` whose data lives under `dir` (no process-env mutation).
    fn startup_in(dir: &Path) -> Startup {
        let mut loaded = cairn_config::load(&LoadOptions {
            cwd: dir.to_path_buf(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        loaded.paths = Paths {
            config_home: dir.join("config"),
            data_home: dir.join("data"),
            state_home: dir.join("state"),
            cache_home: dir.join("cache"),
        };
        Startup {
            loaded,
            quiet: true,
        }
    }

    fn write_session(root: &Path, ws: &str, id: &str, created: &str) -> PathBuf {
        let dir = root.join("sessions").join("abcd");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{{\"v\":1,\"type\":\"header\",\"schema_version\":1,\"session_id\":\"{id}\",\"created_at\":\"{created}\",\"workspace\":\"{ws}\",\"mode\":\"build\",\"model\":\"anthropic/claude-sonnet-4-5\",\"cairn_version\":\"0.1.0\",\"ruleset_version\":null,\"parent_session\":null}}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn scans_headers_and_sorts_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        write_session(
            &tmp.path().join("data"),
            "/ws",
            "ses_a",
            "2026-01-01T00:00:00.000Z",
        );
        write_session(
            &tmp.path().join("data"),
            "/ws",
            "ses_b",
            "2026-02-01T00:00:00.000Z",
        );
        let startup = startup_in(tmp.path());
        let all = scan(&startup);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!(all[0].session_id, "ses_b");
        assert_eq!(all[0].model, "anthropic/claude-sonnet-4-5");
        assert_eq!(all[0].workspace, "/ws");
    }

    #[test]
    fn non_json_files_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions").join("x");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("junk.jsonl"), "not json\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "whatever").unwrap();
        assert!(read_header(&dir.join("junk.jsonl")).is_none());
    }
}
