//! Reading and writing `permissions.json` (SPEC §9.1, REQ-SAFE-003).
//!
//! The file is created `0600` — it records what the user has pre-approved —
//! and every write goes through a temp file and a rename, so a crash leaves
//! the old rules or the new ones, never half of each.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::rule::{compile_all, CompiledRule, Rule, Scope, Skipped};

/// `maxItems` of `rules` (§9.1).
pub const MAX_RULES: usize = 500;

/// A permissions file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum PermError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {reason}")]
    Parse { path: String, reason: String },
    #[error("{path} declares schema_version {found}; this build reads 1")]
    Version { path: String, found: i64 },
    #[error("a rule file holds at most {MAX_RULES} rules")]
    TooMany,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileDoc {
    schema_version: u32,
    rules: Vec<serde_json::Value>,
}

/// What loading a file produced: the usable rules, and why others were not.
#[derive(Debug, Default)]
pub struct Loaded {
    pub rules: Vec<CompiledRule>,
    pub skipped: Vec<Skipped>,
}

/// Load a rules file. A missing file is no rules, not an error; a file that
/// exists and cannot be understood *is* an error, because silently using none
/// of someone's deny rules is the one failure this module must not have.
///
/// # Errors
/// [`PermError`] for an unreadable, non-JSON or wrong-version file.
pub fn load(path: &Path, scope: Scope, case_insensitive: bool) -> Result<Loaded, PermError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(source) => {
            return Err(PermError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| PermError::Parse {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    if version != 1 {
        return Err(PermError::Version {
            path: path.display().to_string(),
            found: version,
        });
    }
    let raw = value
        .get("rules")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (rules, skipped) = compile_all(&raw, scope, case_insensitive);
    Ok(Loaded { rules, skipped })
}

/// Append `rule` to the file, assigning the next free `r<N>` id, and return
/// the stored rule. Existing rules — including ones this build cannot read —
/// are kept byte-for-byte as JSON values.
///
/// # Errors
/// [`PermError`] when the file cannot be read, parsed or written, or is full.
pub fn append(path: &Path, mut rule: Rule) -> Result<Rule, PermError> {
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| PermError::Parse {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
            serde_json::from_value::<FileDoc>(value).map_err(|e| PermError::Parse {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileDoc {
            schema_version: 1,
            rules: Vec::new(),
        },
        Err(source) => {
            return Err(PermError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    if doc.rules.len() >= MAX_RULES {
        return Err(PermError::TooMany);
    }
    let next = doc
        .rules
        .iter()
        .filter_map(|r| r.get("id").and_then(serde_json::Value::as_str))
        .filter_map(|id| id.strip_prefix('r').and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map_or(1, |n| n + 1);
    rule.id = format!("r{next}");
    rule.created_at.get_or_insert_with(|| {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    });
    doc.rules
        .push(serde_json::to_value(&rule).map_err(|e| PermError::Parse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?);
    write_atomic(path, &doc)?;
    Ok(rule)
}

fn write_atomic(path: &Path, doc: &FileDoc) -> Result<(), PermError> {
    let fail = |source: std::io::Error| PermError::Write {
        path: path.display().to_string(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    let text = serde_json::to_string_pretty(doc)
        .map_err(|e| fail(std::io::Error::other(e.to_string())))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(source) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(source));
    }
    Ok(())
}
