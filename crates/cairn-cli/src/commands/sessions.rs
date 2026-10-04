//! `cairn sessions` (SPEC §11.1, §11.7) — a read-only view over the session
//! store: header records only, never a whole file (M1 owns the store itself).

use crate::args::SessionsArgs;
use crate::commands::Startup;
use crate::output::Fail;
use std::io::BufRead;
use std::path::{Path, PathBuf};

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

/// Every stored session, newest first (string-ordered RFC3339 timestamps).
pub fn scan(startup: &Startup) -> Vec<SessionMeta> {
    let root = startup.loaded.paths.sessions_dir();
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for file in inner.flatten() {
                let p = file.path();
                if is_jsonl(&p) {
                    if let Some(meta) = read_header(&p) {
                        out.push(meta);
                    }
                }
            }
        } else if is_jsonl(&path) {
            if let Some(meta) = read_header(&path) {
                out.push(meta);
            }
        }
    }
    out.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then(b.session_id.cmp(&a.session_id))
    });
    out
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// First line of a session file, if it is a usable header record.
pub fn read_header(path: &Path) -> Option<SessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if !v.is_object() {
        return None;
    }
    let get = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let session_id = get("session_id");
    if session_id.is_empty() {
        return None;
    }
    Some(SessionMeta {
        session_id,
        created_at: get("created_at"),
        workspace: get("workspace"),
        mode: get("mode"),
        model: get("model"),
        cairn_version: get("cairn_version"),
        path: path.to_path_buf(),
    })
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
    let wanted_workspace = args.workspace.as_ref().map(|w| {
        std::fs::canonicalize(w)
            .unwrap_or_else(|_| w.clone())
            .to_string_lossy()
            .into_owned()
    });
    let grep = args.grep.as_ref().map(|g| g.to_ascii_lowercase());
    scan(startup)
        .into_iter()
        .filter(|m| match &wanted_workspace {
            Some(w) => {
                std::fs::canonicalize(&m.workspace)
                    .unwrap_or_else(|_| PathBuf::from(&m.workspace))
                    .to_string_lossy()
                    == w.as_str()
            }
            None => true,
        })
        .filter(|m| match &grep {
            Some(g) => format!(
                "{} {} {} {} {} {}",
                m.session_id, m.created_at, m.workspace, m.mode, m.model, m.cairn_version
            )
            .to_ascii_lowercase()
            .contains(g),
            None => true,
        })
        .take(args.limit.max(1))
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
                "{{\"v\":1,\"type\":\"header\",\"schema_version\":1,\"session_id\":\"{id}\",\"created_at\":\"{created}\",\"workspace\":\"{ws}\",\"mode\":\"build\",\"model\":\"anthropic/claude-sonnet-4-5\",\"cairn_version\":\"0.1.0\"}}\n"
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
