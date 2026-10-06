//! `cairn config <sub>` (SPEC §11.1, §11.4, REQ-CLI-003/005/006/007).
//!
//! Reading side goes through `cairn-config` (layers, fallback, effective view);
//! writing side edits one TOML document with `toml_edit` so comments and layout
//! survive, and never persists a value that fails validation.

use crate::args::ConfigCmd;
use crate::commands::Startup;
use crate::output::Fail;
use cairn_config::{effective, validate, Config, Ctx, EffectiveEntry};
use cairn_core::error::codes;
use std::path::{Path, PathBuf};

pub fn run(cmd: &ConfigCmd, startup: &Startup) -> Result<i32, Fail> {
    match cmd {
        ConfigCmd::Get {
            key,
            effective: ann,
        } => get(startup, key, *ann),
        ConfigCmd::List {
            json,
            json_schema,
            effective: ann,
        } => list(startup, *json, *json_schema, *ann),
        ConfigCmd::Validate => unreachable!("`config validate` runs before dispatch"),
        ConfigCmd::Path {
            user,
            project,
            system,
        } => path(startup, *user, *project, *system),
        ConfigCmd::Set {
            key,
            value,
            project,
        } => set(startup, key, value, *project),
        ConfigCmd::Unset { key, project } => unset(startup, key, *project),
        ConfigCmd::Edit => edit(startup),
    }
}

// ------------------------------------------------------------------- reading

fn get(startup: &Startup, key: &str, annotate: bool) -> Result<i32, Fail> {
    let key = key.trim();
    let entries = effective(&startup.loaded);
    if let Some(entry) = entries.iter().find(|e| e.path == key) {
        say!("{}", display(&entry.value, false));
        if annotate {
            say!("# source: {}", annotation(entry));
        }
        return Ok(0);
    }
    if let Some(subtree) = subtree(&entries, key) {
        say!("{}", serde_json::to_string_pretty(&subtree).expect("value"));
        if annotate {
            say!("# source: {}", subtree_source(&entries, key));
        }
        return Ok(0);
    }
    Err(unknown_key(key, &entries))
}

fn list(startup: &Startup, json: bool, json_schema: bool, annotate: bool) -> Result<i32, Fail> {
    if json_schema {
        say!("{}", cairn_config::json_schema_pretty());
        return Ok(0);
    }
    let entries = effective(&startup.loaded);
    if json {
        let rows: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "path": e.path,
                    "value": e.value,
                    "source": e.source,
                    "detail": e.detail,
                })
            })
            .collect();
        say!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(rows)).expect("rows")
        );
        return Ok(0);
    }
    for e in &entries {
        if annotate {
            say!(
                "{} = {}  # {}",
                e.path,
                display(&e.value, true),
                annotation(e)
            );
        } else {
            say!("{} = {}", e.path, display(&e.value, true));
        }
    }
    Ok(0)
}

fn path(startup: &Startup, user: bool, project: bool, system: bool) -> Result<i32, Fail> {
    let paths = &startup.loaded.paths;
    let exclusive = user || project || system;
    if user || !exclusive {
        say!("user: {}", paths.user_config_file().display());
    }
    if project || !exclusive {
        say!("project: {}", project_config_file(startup).display());
    }
    if system || !exclusive {
        say!(
            "system: {}",
            cairn_config::Paths::system_config_file().display()
        );
    }
    Ok(0)
}

/// `cairn config validate` — every problem at once (REQ-CLI-003).
pub fn validate_cmd(loaded: &cairn_config::Loaded, quiet: bool) -> Result<i32, Fail> {
    let mut issues = loaded.issues.clone();
    issues.extend(cairn_config::keybindings::issues(&loaded.paths));
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for issue in &issues {
        if issue.code.starts_with('E') {
            errors += 1;
            yell!("error: {}", issue.render());
        } else {
            warnings += 1;
            if !quiet {
                yell!("warning: {}", issue.render());
            }
        }
    }
    if errors > 0 {
        if !quiet {
            yell!("{errors} error(s), {warnings} warning(s)");
        }
        return Ok(2);
    }
    if !quiet {
        if warnings > 0 {
            say!("config: OK ({warnings} warning(s))");
        } else {
            say!("config: OK");
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------- writing

fn set(startup: &Startup, key: &str, raw: &str, project: bool) -> Result<i32, Fail> {
    let parts = split_key(key)?;
    let file = target_file(startup, project);
    let text = read_config(&file);
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| toml_parse_fail(&file, &toml_error_first_line(&e)))?;
    let value = parse_value(raw)?;
    insert_value(&mut doc, &parts, value, &file)?;
    check_config(&doc.to_string(), &parts, &file, &text)?;
    write_config(&file, &doc.to_string())?;
    say!("{}: {} = {}", file.display(), parts.join("."), raw);
    Ok(0)
}

fn unset(startup: &Startup, key: &str, project: bool) -> Result<i32, Fail> {
    let parts = split_key(key)?;
    let file = target_file(startup, project);
    if !file.exists() {
        return Err(unknown_file_key(key, &file));
    }
    let text = read_config(&file);
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| toml_parse_fail(&file, &toml_error_first_line(&e)))?;
    if !remove_value(&mut doc, &parts)? {
        return Err(unknown_file_key(key, &file));
    }
    check_config(&doc.to_string(), &parts, &file, &text)?;
    write_config(&file, &doc.to_string())?;
    say!("{}: removed {}", file.display(), parts.join("."));
    Ok(0)
}

fn edit(startup: &Startup) -> Result<i32, Fail> {
    let file = target_file(startup, false);
    if !file.exists() {
        write_config(
            &file,
            "# Cairn configuration (SPEC §11.4.1)\n\
             # `cairn config list --json-schema` prints the full schema.\n\
             schema_version = 1\n",
        )?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&file)
        .status()
        .map_err(|e| {
            Fail::usage(
                format!("could not run editor `{editor}`: {e}"),
                "set $VISUAL or $EDITOR to an editor that exists".to_string(),
            )
        })?;
    if !status.success() {
        return Err(Fail::usage(
            format!("editor `{editor}` exited with {status}"),
            "the file may be untouched; run `cairn config validate` to be sure".to_string(),
        ));
    }
    Ok(0)
}

// ------------------------------------------------------------------- helpers

pub(crate) fn target_file(startup: &Startup, project: bool) -> PathBuf {
    if project {
        project_config_file(startup)
    } else {
        startup.loaded.paths.user_config_file()
    }
}

fn project_config_file(startup: &Startup) -> PathBuf {
    startup.workspace().join(".cairn").join("config.toml")
}

pub(crate) fn read_config(file: &Path) -> String {
    std::fs::read_to_string(file).unwrap_or_default()
}

/// Write `<file>` through a sibling temp file, then rename (no torn configs).
pub(crate) fn write_config(file: &Path, text: &str) -> Result<(), Fail> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            Fail::new(
                codes::FS_PERM,
                cairn_core::error::ExitStatus::Generic,
                format!("could not create {}: {e}", dir.display()),
                "check permissions on the config directory".to_string(),
            )
        })?;
    }
    let tmp = file.with_extension(format!("toml.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, file))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            Fail::new(
                codes::FS_PERM,
                cairn_core::error::ExitStatus::Generic,
                format!("could not write {}: {e}", file.display()),
                "check permissions on the config file (SPEC §11.6)".to_string(),
            )
        })
}

/// `a.b.c` (segments may be quoted); rejects empty or malformed keys.
fn split_key(key: &str) -> Result<Vec<String>, Fail> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Fail::usage(
            "empty config key",
            "example: `cairn config set ui.frame_rate 120`".to_string(),
        ));
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in key.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '.' => parts.push(std::mem::take(&mut cur)),
            Some(_) | None => cur.push(ch),
        }
    }
    parts.push(cur);
    for p in &mut parts {
        *p = p.trim().to_string();
    }
    if quote.is_some() {
        return Err(Fail::usage(
            format!("unbalanced quote in key `{key}`"),
            None,
        ));
    }
    for p in &parts {
        if p.is_empty() {
            return Err(Fail::usage(
                format!("malformed key `{key}` (empty segment)"),
                "keys are dotted paths: `section.key`".to_string(),
            ));
        }
        if !p
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return Err(Fail::usage(
                format!("invalid key segment `{p}`"),
                "use ASCII letters, digits, `_` and `-` (table names are plain identifiers)"
                    .to_string(),
            ));
        }
    }
    Ok(parts)
}

/// Accept TOML syntax for the value; anything else is stored as a string.
fn parse_value(raw: &str) -> Result<toml_edit::Value, Fail> {
    let raw = raw.trim();
    match format!("v = {raw}").parse::<toml::Value>() {
        Ok(toml::Value::Table(t)) => match t.get("v") {
            Some(v) => Ok(toml_value_to_edit(v)),
            None => Ok(toml_edit::Value::from(raw)),
        },
        Ok(v) => Ok(toml_value_to_edit(&v)),
        Err(_) => Ok(toml_edit::Value::from(raw)),
    }
}

fn toml_value_to_edit(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        toml::Value::Integer(i) => toml_edit::Value::from(*i),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Datetime(d) => toml_edit::Value::from(d.to_string()),
        toml::Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for i in items {
                arr.push(toml_value_to_edit(i));
            }
            arr.into()
        }
        toml::Value::Table(t) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, val) in t {
                table.insert(k.as_str(), toml_value_to_edit(val));
            }
            table.into()
        }
    }
}

pub(crate) fn insert_value(
    doc: &mut toml_edit::DocumentMut,
    parts: &[String],
    value: toml_edit::Value,
    file: &Path,
) -> Result<(), Fail> {
    let (last, parents) = parts.split_last().expect("split_key returns ≥1 part");
    let mut table = doc.as_table_mut();
    for seg in parents {
        let item = table.entry(seg).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        });
        match item {
            toml_edit::Item::Table(t) => table = t,
            _ => {
                return Err(Fail::new(
                    codes::CFG_BADVALUE,
                    cairn_core::error::ExitStatus::Usage,
                    format!(
                        "`{seg}` in {} is a value, not a table — cannot set `{}`",
                        file.display(),
                        parts.join(".")
                    ),
                    Some(format!("unset `{seg}` first, or pick a different key")),
                ));
            }
        }
    }
    if table
        .get(last)
        .is_some_and(toml_edit::Item::is_array_of_tables)
    {
        return Err(Fail::new(
            codes::CFG_BADVALUE,
            cairn_core::error::ExitStatus::Usage,
            format!(
                "`{}` is an array of tables in {}",
                parts.join("."),
                file.display()
            ),
            Some(
                "array-of-tables entries are managed by `cairn mcp add`/`cairn mcp remove`"
                    .to_string(),
            ),
        ));
    }
    table.insert(last, toml_edit::Item::Value(value));
    Ok(())
}

pub(crate) fn remove_value(
    doc: &mut toml_edit::DocumentMut,
    parts: &[String],
) -> Result<bool, Fail> {
    let (last, parents) = parts.split_last().expect("split_key returns ≥1 part");
    let mut table = doc.as_table_mut();
    for seg in parents {
        match table.get_mut(seg) {
            Some(toml_edit::Item::Table(t)) => table = t,
            _ => return Ok(false),
        }
    }
    Ok(table.remove(last).is_some())
}

/// Re-parse the edited document as `config.toml` and refuse anything invalid
/// (SPEC §11.4.2 — strict schema, ranges, regexes, registry).
pub(crate) fn check_config(
    text: &str,
    parts: &[String],
    file: &Path,
    old_text: &str,
) -> Result<(), Fail> {
    let cfg: Config = match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => return Err(schema_fail(&e, file, parts, text, old_text)),
    };
    let key = parts.join(".");

    // §4.9: `models_path` must name a registry Cairn can read, or the value
    // written here is one the next `load()` discards with `W-CFG-FALLBACK`.
    // Checked only for the key being written, so a stale broken override does
    // not block unrelated `cairn config set` calls.
    if key == "models_path" && !cfg.models_path.trim().is_empty() {
        let get: cairn_config::EnvLookup<'_> = &|k| std::env::var(k).ok();
        let path = cairn_config::expand_tilde(cfg.models_path.trim(), get);
        if let Err(reason) = cairn_config::registry_from_path(&path) {
            return Err(Fail::new(
                codes::CFG_BADVALUE,
                cairn_core::error::ExitStatus::Usage,
                format!("{}: {reason}", file.display()),
                Some("value not written; point models_path at a readable models.json".to_string()),
            ));
        }
    }

    for issue in validate(&cfg, &Ctx::default()) {
        let touches = issue
            .path
            .as_deref()
            .is_some_and(|p| p == key || key.starts_with(&format!("{p}.")));
        if touches && issue.code.starts_with('E') {
            return Err(Fail::new(
                issue.code,
                cairn_core::error::ExitStatus::Usage,
                format!("{}: {}", file.display(), issue.message),
                Some("value not written; pick a valid one and retry".to_string()),
            ));
        }
    }
    Ok(())
}

fn schema_fail(
    e: &toml::de::Error,
    file: &Path,
    parts: &[String],
    text: &str,
    old_text: &str,
) -> Fail {
    let msg = e.message().to_string();
    let code = if msg.contains("unknown field") {
        codes::CFG_UNKNOWN
    } else if msg.contains("duplicate key") {
        codes::CFG_DUPNAME
    } else {
        codes::CFG_BADVALUE
    };
    let location = match e.span() {
        Some(span) if !text[..span.start.min(text.len())].is_empty() => {
            format!("{}:{}", file.display(), text_linecol(text, span.start))
        }
        _ => match e.span() {
            Some(span) => format!("{}:{}", file.display(), text_linecol(old_text, span.start)),
            None => file.display().to_string(),
        },
    };
    Fail::new(
        code,
        cairn_core::error::ExitStatus::Usage,
        format!("{location}: {msg} (key `{}`)", parts.join(".")),
        Some(
            "see `cairn config list --json-schema` for valid keys; nothing was written".to_string(),
        ),
    )
}

/// Line number (1-based) of a byte offset in the document.
fn text_linecol(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
}

/// First line of a `toml_edit` parse error (its Display is multi-line).
fn toml_error_first_line(e: &toml_edit::TomlError) -> String {
    e.to_string()
        .lines()
        .next()
        .unwrap_or("invalid TOML")
        .trim()
        .to_string()
}

pub(crate) fn toml_parse_fail(file: &Path, message: &str) -> Fail {
    Fail::new(
        codes::CFG_BADVALUE,
        cairn_core::error::ExitStatus::Usage,
        format!("{}: {message}", file.display()),
        Some("the file is not valid TOML; fix it by hand or with `cairn config edit`".to_string()),
    )
}

fn unknown_key(key: &str, entries: &[EffectiveEntry]) -> Fail {
    let mut hint = "run `cairn config list` to see every key".to_string();
    if let Some(near) = nearest(key, entries) {
        hint = format!("did you mean `{near}`? otherwise run `cairn config list`");
    }
    Fail::new(
        codes::CFG_UNKNOWN,
        cairn_core::error::ExitStatus::Usage,
        format!("unknown config key `{key}`"),
        Some(hint),
    )
}

fn unknown_file_key(key: &str, file: &Path) -> Fail {
    Fail::new(
        codes::CFG_UNKNOWN,
        cairn_core::error::ExitStatus::Usage,
        format!("`{key}` is not set in {}", file.display()),
        Some(
            "unset only removes a key from one layer; `cairn config get <key>` shows the effective value"
                .to_string(),
        ),
    )
}

fn nearest(key: &str, entries: &[EffectiveEntry]) -> Option<String> {
    let target = key.to_ascii_lowercase();
    entries
        .iter()
        .map(|e| e.path.clone())
        .find(|p| p.to_ascii_lowercase() == target || p.starts_with(&target))
}

/// Leaf display: strings raw for `get`, quoted for `list` (TOML-shaped).
fn display(value: &serde_json::Value, quote_strings: bool) -> String {
    match value {
        serde_json::Value::String(s) if !quote_strings => s.clone(),
        other => serde_json::to_string(other).expect("value"),
    }
}

/// Winning layer annotation (`REQ-CLI-006`): `user`, `flag(-m)`, `env(CAIRN_MODE)`.
fn annotation(e: &EffectiveEntry) -> String {
    match &e.detail {
        Some(d) => format!("{}({d})", e.source),
        None => e.source.to_string(),
    }
}

/// Representative annotation for a whole subtree.
fn subtree_source(entries: &[EffectiveEntry], prefix: &str) -> String {
    let with_prefix = format!("{prefix}.");
    entries
        .iter()
        .find(|e| e.path.starts_with(&with_prefix))
        .map_or_else(|| "default".to_string(), annotation)
}

/// Leaf paths under `prefix` assembled into one JSON object.
fn subtree(entries: &[EffectiveEntry], prefix: &str) -> Option<serde_json::Value> {
    let mut root = serde_json::Map::new();
    let mut found = false;
    for e in entries {
        let Some(rest) = e
            .path
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('.'))
        else {
            continue;
        };
        found = true;
        let segs: Vec<&str> = rest.split('.').collect();
        let mut cur = &mut root;
        for (i, seg) in segs.iter().enumerate() {
            if i + 1 == segs.len() {
                cur.insert(seg.to_string(), e.value.clone());
                break;
            }
            let entry = cur
                .entry(seg.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = serde_json::Value::Object(serde_json::Map::new());
            }
            cur = entry.as_object_mut().expect("just made it an object");
        }
    }
    found.then(|| serde_json::Value::Object(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{Issue, SCHEMA_VERSION};

    #[test]
    fn split_key_handles_quotes_and_rejects_junk() {
        assert_eq!(
            split_key("ui.frame_rate").unwrap(),
            vec!["ui", "frame_rate"]
        );
        assert_eq!(split_key(" a . b ").unwrap(), vec!["a", "b"]);
        assert!(split_key("").is_err());
        assert!(split_key("a..b").is_err());
        assert!(split_key("a b.c").is_err());
    }

    #[test]
    fn parse_value_prefers_toml_and_falls_back_to_string() {
        assert_eq!(parse_value("60").unwrap().as_integer(), Some(60));
        assert_eq!(parse_value("true").unwrap().as_bool(), Some(true));
        assert_eq!(parse_value("plan").unwrap().as_str(), Some("plan"));
        assert_eq!(
            parse_value(r#"["a","b"]"#)
                .unwrap()
                .as_array()
                .map(toml_edit::Array::len),
            Some(2)
        );
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mut doc: toml_edit::DocumentMut = "schema_version = 1\n".parse().unwrap();
        let parts = vec!["ui".to_string(), "frame_rate".to_string()];
        insert_value(
            &mut doc,
            &parts,
            toml_edit::Value::from(120),
            Path::new("config.toml"),
        )
        .unwrap();
        let text = doc.to_string();
        assert!(text.contains("frame_rate"), "{text}");
        let cfg: Config = toml::from_str(&text).expect("edited text is a valid config");
        assert_eq!(cfg.ui.frame_rate, 120);
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert!(remove_value(&mut doc, &parts).unwrap());
        assert!(!remove_value(&mut doc, &parts).unwrap());
    }

    #[test]
    fn insert_refuses_array_of_tables() {
        let text = "[[mcp.servers]]\nname = \"x\"\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        let parts = vec!["mcp".to_string(), "servers".to_string()];
        let err = insert_value(
            &mut doc,
            &parts,
            toml_edit::Value::from("x"),
            Path::new("config.toml"),
        )
        .unwrap_err();
        assert_eq!(err.code, codes::CFG_BADVALUE);
    }

    #[test]
    fn subtree_builds_objects() {
        let entries = vec![
            EffectiveEntry {
                path: "ui.frame_rate".into(),
                value: serde_json::json!(60),
                source: "default",
                detail: None,
            },
            EffectiveEntry {
                path: "ui.theme".into(),
                value: serde_json::json!("cairn-dark"),
                source: "default",
                detail: None,
            },
        ];
        let v = subtree(&entries, "ui").unwrap();
        assert_eq!(v["frame_rate"], 60);
        assert_eq!(v["theme"], "cairn-dark");
        assert!(subtree(&entries, "nope").is_none());
    }

    #[test]
    fn schema_version_is_the_spec_value() {
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn issue_render_includes_code_and_location() {
        let i = Issue::error(codes::CFG_RANGE, "out of range", Some("a.b"), "ui").at(
            "/tmp/config.toml",
            4,
            2,
        );
        assert_eq!(
            i.render(),
            "/tmp/config.toml:4:2: E-CFG-RANGE: out of range"
        );
    }
}
