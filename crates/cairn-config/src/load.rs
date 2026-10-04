//! Layered configuration loading (SPEC §11.5, §11.6).

use crate::issue::{Issue, Source};
use crate::model::Config;
use crate::paths::{discover_workspace, Paths};
use crate::validate::{validate, Ctx};
use cairn_core::error::codes;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Flags that override configuration (SPEC §11.1, precedence rank 1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlagOverrides {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub profile: Option<String>,
    pub output: Option<String>,
    pub log_level: Option<String>,
    pub log_file: Option<String>,
    pub trace: Option<bool>,
    pub offline: Option<bool>,
    pub quiet: Option<bool>,
    pub no_color: Option<bool>,
    pub no_update_check: Option<bool>,
    pub allow_unsafe: Option<bool>,
    pub max_iterations: Option<u32>,
    pub approve_plan: Option<bool>,
    pub verbose: u8,
}

impl FlagOverrides {
    /// `true` when no flag was provided (cheap check).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Everything `load()` needs. `env: None` reads the process environment.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub cwd: PathBuf,
    /// `--workspace`.
    pub workspace: Option<PathBuf>,
    /// `-c/--config`.
    pub explicit_config: Option<PathBuf>,
    /// `--profile`.
    pub profile: Option<String>,
    pub flags: FlagOverrides,
    pub env: Option<BTreeMap<String, String>>,
    /// `--allow-unknown-keys` (SPEC §11.4.2).
    pub allow_unknown_keys: bool,
    /// TTY state of the output stream, when known.
    pub is_tty: Option<bool>,
}

/// One layer file that was considered.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerInfo {
    pub path: PathBuf,
    pub source: Source,
    pub exists: bool,
    pub bytes: u64,
}

/// Result of [`load`].
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Winning layer per dotted key (REQ-CLI-006).
    pub sources: BTreeMap<String, Source>,
    pub layers: Vec<LayerInfo>,
    pub issues: Vec<Issue>,
    pub workspace: PathBuf,
    pub paths: Paths,
    /// `env`/`flag` keys that failed to parse (exit 2 at startup).
    pub parse_failures: usize,
}

impl Loaded {
    /// Errors that make startup fail with exit 2 (REQ-CLI-004).
    #[must_use]
    pub fn fatal_issues(&self) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|i| i.fatal && i.code.starts_with('E'))
            .collect()
    }

    /// Issues to surface as `W-CFG-PARTIAL` warnings (REQ-CLI-004).
    #[must_use]
    pub fn partial_issues(&self) -> Vec<&Issue> {
        self.issues.iter().filter(|i| !i.fatal).collect()
    }
}

// ---------------------------------------------------------------- env mapping

/// How an environment variable value is interpreted (SPEC §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kv {
    Str,
    /// Unit enum parsed through serde (`mode`, `output.format`, `log.level`).
    Enum,
    Bool,
    /// `CAIRN_NO_UPDATE_CHECK=1` → `update.check = false`.
    BoolNeg,
    Int,
    /// Split on `,`.
    ListComma,
    /// Split on `:` (paths).
    ListColon,
}

/// `CAIRN_*` → config key (SPEC §11.3 full enumeration).
pub const ENV_KEYS: &[(&str, &str, Kv)] = &[
    ("CAIRN_MODEL", "model", Kv::Str),
    ("CAIRN_MODE", "mode", Kv::Enum),
    ("CAIRN_PROFILE", "profile", Kv::Str),
    ("CAIRN_OUTPUT", "output.format", Kv::Enum),
    ("CAIRN_LOG_LEVEL", "log.level", Kv::Enum),
    ("CAIRN_LOG_FILE", "log.file", Kv::Str),
    ("CAIRN_TRACE", "trace.enabled", Kv::Bool),
    ("CAIRN_OFFLINE", "network.offline", Kv::Bool),
    ("CAIRN_QUIET", "ui.quiet", Kv::Bool),
    ("CAIRN_SCREEN_READER", "ui.screen_reader", Kv::Bool),
    ("CAIRN_SHELL", "shell.command", Kv::Str),
    ("CAIRN_MAX_ITERATIONS", "auto.max_iterations", Kv::Int),
    ("CAIRN_NO_UPDATE_CHECK", "update.check", Kv::BoolNeg),
    ("CAIRN_VERIFY_COMMANDS", "verify.commands", Kv::ListComma),
    ("CAIRN_CACHE_DIR", "paths.cache_dir", Kv::Str),
    ("CAIRN_DATA_DIR", "paths.data_dir", Kv::Str),
];

/// Parse a boolean per SPEC §11.3 (`E-CFG-BADENV` otherwise).
pub fn parse_bool_env(raw: &str) -> Result<bool, String> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "" => Ok(false),
        other => Err(format!(
            "{other:?} is not a boolean; use 1,true,yes,on or 0,false,no or an empty string"
        )),
    }
}

fn parse_via_serde<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|e| {
        e.to_string()
            .replace("Value::String(", "")
            .replace("),", ",")
    })
}

/// Apply one `key = value` override. Returns `Err` with a human message.
pub fn apply_kv(cfg: &mut Config, path: &str, raw: &str, kind: Kv) -> Result<(), String> {
    let parse_int = |what: &str| -> Result<u64, String> {
        raw.parse::<u64>()
            .map_err(|_| format!("{what} = {raw:?} is not an integer"))
    };
    match (path, kind) {
        ("model", _) => cfg.model = raw.to_string(),
        ("mode", _) => cfg.mode = parse_via_serde(raw)?,
        ("profile", _) => cfg.profile = raw.to_string(),
        ("output.format", _) => cfg.output.format = parse_via_serde(raw)?,
        ("log.level", _) => cfg.log.level = parse_via_serde(raw)?,
        ("log.file", _) => cfg.log.file = raw.to_string(),
        ("trace.enabled", Kv::Bool) => cfg.trace.enabled = parse_bool_env(raw)?,
        ("network.offline", Kv::Bool) => cfg.network.offline = parse_bool_env(raw)?,
        ("ui.quiet", Kv::Bool) => cfg.ui.quiet = parse_bool_env(raw)?,
        ("ui.color", Kv::Bool) => cfg.ui.color = Some(parse_bool_env(raw)?),
        ("ui.screen_reader", Kv::Bool) => cfg.ui.screen_reader = parse_bool_env(raw)?,
        ("shell.command", _) => cfg.shell.command = raw.to_string(),
        ("auto.max_iterations", Kv::Int) => {
            cfg.auto.max_iterations =
                u32::try_from(parse_int("auto.max_iterations")?).map_err(|_| {
                    format!("auto.max_iterations = {raw:?} is larger than {}", u32::MAX)
                })?;
        }
        ("update.check", Kv::Bool) => cfg.update.check = parse_bool_env(raw)?,
        ("update.check", Kv::BoolNeg) => cfg.update.check = !parse_bool_env(raw)?,
        ("verify.commands", Kv::ListComma) => {
            cfg.verify.commands = raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        ("verify.commands", Kv::ListColon) => {
            cfg.verify.commands = raw
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        ("paths.cache_dir", _) => cfg.paths.cache_dir = raw.to_string(),
        ("paths.data_dir", _) => cfg.paths.data_dir = raw.to_string(),
        ("plans.auto_approve", Kv::Bool) => cfg.plans.auto_approve = parse_bool_env(raw)?,
        ("modes.allow_unsafe", Kv::Bool) => cfg.modes.allow_unsafe = parse_bool_env(raw)?,
        (other, _) => return Err(format!("unsupported config key {other}")),
    }
    Ok(())
}

// ------------------------------------------------------------------- merging

/// Split `a.b."c d".e` into segments (quotes stripped).
fn split_key(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in key.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '.' if !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn table_get<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let segs = split_key(key);
    let mut cur = table.get(&segs[0])?;
    for seg in &segs[1..] {
        cur = cur.as_table()?.get(seg)?;
    }
    Some(cur)
}

fn table_set(table: &mut toml::Table, key: &str, value: toml::Value) {
    let segs = split_key(key);
    let mut cur = table;
    for seg in &segs[..segs.len().saturating_sub(1)] {
        cur = cur
            .entry(seg.as_str())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("non-table value on key path");
    }
    cur.insert(segs.last().expect("key").clone(), value);
}

fn table_remove(table: &mut toml::Table, key: &str) -> bool {
    let segs = split_key(key);
    let mut cur = table;
    for seg in &segs[..segs.len().saturating_sub(1)] {
        match cur.get_mut(seg).and_then(toml::Value::as_table_mut) {
            Some(t) => cur = t,
            None => return false,
        }
    }
    cur.remove(segs.last().expect("key")).is_some()
}

/// Collect every dotted path present in a layer (used for source tracking and
/// per-layer fallback decisions).
fn walk_paths(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (k, v) in table {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => {
                out.push(path.clone());
                walk_paths(t, &path, out);
            }
            _ => out.push(path),
        }
    }
}

/// Deep merge `layer` into `merged` (SPEC §11.5): tables merge key-wise,
/// scalars and arrays replace.
fn merge_tables(merged: &mut toml::Table, layer: &toml::Table) {
    for (k, v) in layer {
        match (merged.get_mut(k), v) {
            (Some(toml::Value::Table(mt)), toml::Value::Table(lt)) => merge_tables(mt, lt),
            _ => {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Comment out `key += [...]` lines (preserving line numbers) and return them.
fn extract_appends(text: &str) -> (String, Vec<(String, String, usize)>) {
    let mut lines_out = Vec::new();
    let mut appends = Vec::new();
    let key_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '"' | '\''))
    };
    for (idx, line) in text.split('\n').enumerate() {
        let trimmed = line.trim_start();
        let is_comment = trimmed.starts_with('#');
        if !is_comment {
            if let Some(pos) = line.find("+=") {
                let key = line[..pos].trim();
                let rhs = line[pos + 2..].trim();
                if key_ok(key) && rhs.starts_with('[') && rhs.ends_with(']') {
                    lines_out.push(format!("# cairn-append: {line}"));
                    appends.push((key.to_string(), rhs.to_string(), idx + 1));
                    continue;
                }
            }
        }
        lines_out.push(line.to_string());
    }
    (lines_out.join("\n"), appends)
}

fn parse_array(rhs: &str) -> Result<toml::Value, String> {
    let doc: toml::Value = toml::from_str(&format!("v = {rhs}")).map_err(|e| e.to_string())?;
    Ok(doc
        .get("v")
        .cloned()
        .unwrap_or(toml::Value::Array(Vec::new())))
}

fn dedup_extend(lower: &mut Vec<toml::Value>, extra: Vec<toml::Value>) {
    for v in extra {
        if !lower.contains(&v) {
            lower.push(v);
        }
    }
}

fn line_col_of(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in text.char_indices() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Dotted key paths present in a TOML text, with their 1-based line numbers.
fn key_paths_in_text(text: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut table: Vec<String> = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with("[[") && t.ends_with("]]") {
            table = vec![t[2..t.len() - 2].trim().replace('"', "")];
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            table = vec![t[1..t.len() - 1].trim().replace('"', "")];
            continue;
        }
        if let Some(pos) = t.find('=') {
            let key = t[..pos].trim().replace('"', "");
            if key.is_empty() || key.contains('+') {
                continue;
            }
            let full = if table.is_empty() {
                key
            } else {
                format!("{}.{}", table.join("."), key)
            };
            out.push((i + 1, full));
        }
    }
    out
}

/// Line number of a dotted key (or of its table header) in a layer text.
fn find_key_line(text: &str, path: &str) -> Option<usize> {
    let entries = key_paths_in_text(text);
    if let Some((line, _)) = entries.iter().find(|(_, p)| p == path) {
        return Some(*line);
    }
    // fall back to the table header that owns the key
    text.split('\n')
        .enumerate()
        .find(|(_, l)| {
            let t = l.trim();
            (t.starts_with('[') && t.ends_with(']')) && {
                let name = t
                    .trim_start_matches("[[")
                    .trim_start_matches('[')
                    .trim_end_matches("]]")
                    .trim_end_matches(']');
                name.trim().replace('"', "") == path
            }
        })
        .map(|(i, _)| i + 1)
}

/// Find the single key whose removal makes the layer parse: removes one culprit
/// per iteration so `config validate` can name it precisely (REQ-CLI-003).
fn find_culprit(base: &toml::Table, layer: &toml::Table) -> Option<String> {
    let mut paths: Vec<String> = Vec::new();
    walk_paths(base, "", &mut paths);
    for path in paths.into_iter().rev() {
        let mut candidate = layer.clone();
        table_remove(&mut candidate, &path);
        if let Ok(text) = toml::to_string(&candidate) {
            if toml::from_str::<Config>(&text).is_ok() {
                return Some(path);
            }
        }
    }
    None
}

// --------------------------------------------------------------------- load

fn process_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

/// Load the full configuration stack. Never fails fast: all problems are
/// returned in `issues` (REQ-CLI-003).
#[must_use]
pub fn load(opts: &LoadOptions) -> Loaded {
    let env = opts.env.clone().unwrap_or_else(process_env);
    let get = |k: &str| env.get(k).cloned();
    let mut paths = Paths::resolve(&get);
    let workspace = discover_workspace(&opts.cwd, opts.workspace.as_deref(), &get);

    let mut issues: Vec<Issue> = Vec::new();
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let mut merged = toml::Table::new();
    let mut layers: Vec<LayerInfo> = Vec::new();

    // Ascending precedence (SPEC §11.5): system → user → profile → project
    // (farthest parent … workspace root) → explicit --config.
    let mut stack: Vec<(Source, PathBuf)> = vec![
        (Source::System, Paths::system_config_file()),
        (Source::User, paths.user_config_file()),
    ];
    if let Some(p) = opts
        .profile
        .as_deref()
        .or(env.get("CAIRN_PROFILE").map(String::as_str))
    {
        stack.push((
            Source::Profile(p.to_string()),
            paths.config_home.join("profiles").join(format!("{p}.toml")),
        ));
    }
    let mut ancestors: Vec<PathBuf> = workspace.ancestors().map(PathBuf::from).collect();
    ancestors.reverse(); // farthest first, workspace root last (nearest wins)
    for a in ancestors {
        stack.push((Source::Project, a.join(".cairn/config.toml")));
    }
    if let Some(explicit) = &opts.explicit_config {
        stack.push((Source::Explicit, explicit.clone()));
    }

    for (source, path) in &stack {
        let exists = path.is_file();
        let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
        layers.push(LayerInfo {
            path: path.clone(),
            source: source.clone(),
            exists,
            bytes,
        });
        if !exists {
            if matches!(source, Source::Explicit) {
                issues.push(
                    Issue::error(
                        codes::FS_NOTFOUND,
                        format!("config file {} does not exist", path.display()),
                        None,
                        "",
                    )
                    .in_file(path),
                );
            }
            continue;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                issues.push(
                    Issue::error(
                        codes::CFG_BADVALUE,
                        format!("cannot read config: {e}"),
                        None,
                        "",
                    )
                    .in_file(path),
                );
                continue;
            }
        };
        load_layer(
            &text,
            path,
            source,
            &mut merged,
            &mut sources,
            &mut issues,
            opts.allow_unknown_keys,
        );
    }

    // Parse the merged document into the typed config.
    let mut config = match Config::deserialize(toml::Value::Table(merged.clone())) {
        Ok(c) => c,
        Err(e) => {
            issues.push(Issue::error(
                codes::CFG_BADVALUE,
                format!("merged configuration is invalid: {e}"),
                None,
                "",
            ));
            Config::default()
        }
    };

    // Precedence rank 2: environment (SPEC §11.3).
    for (var, path, kind) in ENV_KEYS {
        if let Some(raw) = env.get(*var) {
            match apply_kv(&mut config, path, raw, *kind) {
                Ok(()) => {
                    sources.insert((*path).to_string(), Source::Env((*var).to_string()));
                }
                Err(msg) => {
                    // Booleans/ints have their own code (SPEC §11.3); enums and
                    // anything else use the generic enum/value rule (§11.4.2).
                    let code = match kind {
                        Kv::Bool | Kv::BoolNeg | Kv::Int => codes::CFG_BADENV,
                        Kv::Enum | Kv::Str | Kv::ListComma | Kv::ListColon => codes::CFG_BADVALUE,
                    };
                    issues.push(Issue::error(code, format!("{var}: {msg}"), Some(*path), ""));
                }
            }
        }
    }
    if let Some(v) = env.get("NO_COLOR") {
        if !v.is_empty() && v != "0" {
            config.ui.color = Some(false);
            sources.insert("ui.color".to_string(), Source::Env("NO_COLOR".to_string()));
        }
    }
    if let Some(v) = env.get("CAIRN_VERBOSE") {
        let n: u8 = v.parse().unwrap_or(0);
        if n > 0 {
            config.log.level = match n {
                1 => crate::model::LogLevel::Info,
                2 => crate::model::LogLevel::Debug,
                _ => crate::model::LogLevel::Trace,
            };
            sources.insert(
                "log.level".to_string(),
                Source::Env("CAIRN_VERBOSE".to_string()),
            );
        }
    }

    // Precedence rank 1: CLI flags (SPEC §11.1).
    apply_flags(&mut config, &opts.flags, &mut sources, &mut issues);
    if let Some(p) = &opts.profile {
        config.profile.clone_from(p);
        sources.insert("profile".to_string(), Source::Flag("--profile".to_string()));
    }

    // `paths.*` overrides win over the resolved XDG dirs (SPEC §11.6).
    if !config.paths.data_dir.is_empty() {
        paths.data_home = crate::paths::expand_tilde(&config.paths.data_dir, &get);
    }
    if !config.paths.cache_dir.is_empty() {
        paths.cache_home = crate::paths::expand_tilde(&config.paths.cache_dir, &get);
    }
    if !config.paths.config_dir.is_empty() {
        paths.config_home = crate::paths::expand_tilde(&config.paths.config_dir, &get);
    }

    // Final validation (SPEC §11.4.2) — all issues, not fail-fast.
    let ctx = Ctx {
        is_tty: opts.is_tty,
        themes_dir: Some(paths.config_home.join("themes")),
        registry: None,
    };
    issues.extend(validate(&config, &ctx));

    Loaded {
        config,
        sources,
        layers,
        issues,
        workspace,
        paths,
        parse_failures: 0,
    }
}

/// Comment `+=` lines, validate the layer against the schema, apply `+=`
/// semantics and merge it. Records sources and issues.
fn load_layer(
    text: &str,
    path: &std::path::Path,
    source: &Source,
    merged: &mut toml::Table,
    sources: &mut BTreeMap<String, Source>,
    issues: &mut Vec<Issue>,
    allow_unknown_keys: bool,
) {
    let (commented, appends) = extract_appends(text);

    // 1. Syntax. A file that does not parse cannot be merged at all: report it
    //    with `file:line` and ignore the layer (REQ-CLI-005: never adopt a
    //    broken layer — the next lower layer wins).
    let mut layer = match toml::from_str::<toml::Table>(&commented) {
        Ok(t) => t,
        Err(e) => {
            let (line, column) = e
                .span()
                .map_or((1usize, 1usize), |span| line_col_of(&commented, span.start));
            issues.push(
                Issue::error(codes::CFG_BADVALUE, e.to_string(), None, "")
                    .in_file(path.to_path_buf())
                    .at(path.to_path_buf(), line, column),
            );
            issues.push(
                Issue::warn(
                    codes::CFG_FALLBACK,
                    "file could not be parsed; layer ignored, using the next lower layer",
                    None,
                    "",
                )
                .in_file(path),
            );
            return;
        }
    };

    // `key += [...]` (SPEC §11.5): append + dedup against lower layers.
    for (key, rhs, line) in appends {
        match parse_array(&rhs) {
            Ok(toml::Value::Array(extra)) => {
                let mut lower = match table_get(merged, &key) {
                    Some(toml::Value::Array(a)) => a.clone(),
                    Some(other) => {
                        issues.push(
                            Issue::error(
                                codes::CFG_BADVALUE,
                                format!("{key} += requires a list in the next lower layer, found {other}"),
                                Some(&key),
                                "",
                            )
                            .in_file(path.to_path_buf())
                            .at(path, line, 1),
                        );
                        continue;
                    }
                    None => Vec::new(),
                };
                dedup_extend(&mut lower, extra);
                if let Some(toml::Value::Array(existing)) = table_get(&layer, &key).cloned() {
                    let mut combined = existing;
                    dedup_extend(&mut combined, lower);
                    lower = combined;
                }
                table_set(&mut layer, &key, toml::Value::Array(lower));
            }
            Ok(other) => {
                issues.push(
                    Issue::error(
                        codes::CFG_BADVALUE,
                        format!("{key} += expects an array, found {other}"),
                        Some(&key),
                        "",
                    )
                    .in_file(path.to_path_buf())
                    .at(path, line, 1),
                );
            }
            Err(msg) => {
                issues.push(
                    Issue::error(
                        codes::CFG_BADVALUE,
                        format!("{key} += {msg}"),
                        Some(&key),
                        "",
                    )
                    .in_file(path.to_path_buf())
                    .at(path.to_path_buf(), line, 1),
                );
            }
        }
    }

    // 2. Schema. Find the key that makes the layer invalid, drop it from this
    //    layer and fall back to the next lower layer (REQ-CLI-005), reporting
    //    `file:line` (REQ-CLI-003). Unknown keys stay fatal unless
    //    `--allow-unknown-keys` (SPEC §11.4.2).
    let mut attempts = 0;
    while attempts < 32 {
        attempts += 1;
        let candidate = toml::to_string(&layer).unwrap_or_default();
        let Err(err) = toml::from_str::<Config>(&candidate) else {
            break;
        };
        let msg = err.to_string();
        let unknown = msg.contains("unknown field");
        let Some(culprit) = find_culprit(&layer, &layer) else {
            let code = if unknown {
                codes::CFG_UNKNOWN
            } else {
                codes::CFG_BADVALUE
            };
            let mut issue = Issue::error(code, msg, None, "").in_file(path.to_path_buf());
            issue.fatal = unknown;
            issues.push(issue);
            break;
        };
        let line = find_key_line(&commented, &culprit);
        let mut issue = Issue::error(
            if unknown {
                codes::CFG_UNKNOWN
            } else {
                codes::CFG_BADVALUE
            },
            msg,
            Some(&culprit),
            "",
        )
        .in_file(path.to_path_buf());
        issue.fatal = unknown && !allow_unknown_keys;
        if let Some(l) = line {
            issue = issue.at(path.to_path_buf(), l, 1);
        }
        if !table_remove(&mut layer, &culprit) {
            issues.push(issue);
            break;
        }
        issues.push(issue);
        if unknown && allow_unknown_keys {
            issues.push(
                Issue::warn(
                    codes::CFG_FALLBACK,
                    format!("unknown key {culprit} ignored (--allow-unknown-keys)"),
                    Some(&culprit),
                    "",
                )
                .in_file(path),
            );
        } else if !unknown {
            issues.push(
                Issue::warn(
                    codes::CFG_FALLBACK,
                    format!("invalid value for {culprit} ignored, using the next lower layer"),
                    Some(&culprit),
                    "",
                )
                .in_file(path),
            );
        }
    }

    // 3. Cross-field rules (SPEC §11.4.2) run against this layer alone: values
    //    this layer gets wrong are dropped so the lower layer's value survives.
    if let Ok(partial) = Config::deserialize(toml::Value::Table(layer.clone())) {
        let mut layer_paths = Vec::new();
        walk_paths(&layer, "", &mut layer_paths);
        for mut iss in validate(&partial, &Ctx::default()) {
            let Some(ikey) = iss.path.clone() else {
                continue;
            };
            let owned = layer_paths.iter().any(|p| {
                p == &ikey
                    || p.starts_with(&format!("{ikey}."))
                    || ikey.starts_with(&format!("{p}."))
            });
            if !owned {
                continue;
            }
            let before = layer.clone();
            table_remove(&mut layer, &ikey);
            if layer == before {
                continue;
            }
            // Report the value the user actually wrote (with its location) as a
            // fallback rather than a startup failure (REQ-CLI-004/005).
            iss.fatal = false;
            iss = iss.in_file(path.to_path_buf());
            if let Some(l) = find_key_line(&commented, &ikey) {
                iss = iss.at(path.to_path_buf(), l, 1);
            }
            issues.push(iss);
            issues.push(
                Issue::warn(
                    codes::CFG_FALLBACK,
                    format!("invalid value for {ikey} ignored, using the next lower layer"),
                    Some(&ikey),
                    "",
                )
                .in_file(path),
            );
        }
    }

    // Record the winning layer for every key this layer sets, then merge.
    let mut paths_set = Vec::new();
    walk_paths(&layer, "", &mut paths_set);
    for p in paths_set {
        sources.insert(p, source.clone());
    }
    merge_tables(merged, &layer);
}

fn flag_set(
    config: &mut Config,
    sources: &mut BTreeMap<String, Source>,
    issues: &mut Vec<Issue>,
    path: &str,
    flag: &str,
    raw: &str,
    kind: Kv,
) {
    match apply_kv(config, path, raw, kind) {
        Ok(()) => {
            sources.insert(path.to_string(), Source::Flag(flag.to_string()));
        }
        Err(msg) => issues.push(Issue::error(codes::CFG_BADVALUE, msg, Some(path), "")),
    }
}

/// One row of the §11.1 boolean-flag table: `(config path, flag, override
/// value, getter, setter)`.
type BoolFlagSpec = (
    &'static str,
    &'static str,
    Option<bool>,
    fn(&mut Config) -> bool,
    fn(&mut Config, bool),
);

fn apply_flags(
    config: &mut Config,
    flags: &FlagOverrides,
    sources: &mut BTreeMap<String, Source>,
    issues: &mut Vec<Issue>,
) {
    if let Some(v) = &flags.model {
        flag_set(config, sources, issues, "model", "-m", v, Kv::Str);
    }
    if let Some(v) = &flags.mode {
        flag_set(config, sources, issues, "mode", "--mode", v, Kv::Str);
    }
    if let Some(v) = &flags.output {
        flag_set(
            config,
            sources,
            issues,
            "output.format",
            "--output",
            v,
            Kv::Str,
        );
    }
    if let Some(v) = &flags.log_level {
        flag_set(
            config,
            sources,
            issues,
            "log.level",
            "--log-level",
            v,
            Kv::Str,
        );
    }
    if let Some(v) = &flags.log_file {
        flag_set(
            config,
            sources,
            issues,
            "log.file",
            "--log-file",
            v,
            Kv::Str,
        );
    }
    if let Some(v) = flags.max_iterations {
        flag_set(
            config,
            sources,
            issues,
            "auto.max_iterations",
            "--max-iterations",
            &v.to_string(),
            Kv::Int,
        );
    }
    let bool_flags: [BoolFlagSpec; 7] = [
        (
            "trace.enabled",
            "--trace",
            flags.trace,
            |c| c.trace.enabled,
            |c, v| c.trace.enabled = v,
        ),
        (
            "network.offline",
            "--offline",
            flags.offline,
            |c| c.network.offline,
            |c, v| c.network.offline = v,
        ),
        (
            "ui.quiet",
            "-q",
            flags.quiet,
            |c| c.ui.quiet,
            |c, v| c.ui.quiet = v,
        ),
        (
            "ui.color",
            "--no-color",
            flags.no_color.map(|n| !n),
            |c| c.ui.color.unwrap_or(true),
            |c, v| c.ui.color = Some(v),
        ),
        (
            "update.check",
            "--no-update-check",
            flags.no_update_check.map(|n| !n),
            |c| c.update.check,
            |c, v| c.update.check = v,
        ),
        (
            "modes.allow_unsafe",
            "--dangerously-skip-permissions",
            flags.allow_unsafe,
            |c| c.modes.allow_unsafe,
            |c, v| c.modes.allow_unsafe = v,
        ),
        (
            "plans.auto_approve",
            "--approve-plan",
            flags.approve_plan,
            |c| c.plans.auto_approve,
            |c, v| c.plans.auto_approve = v,
        ),
    ];
    for (path, flag, value, get, put) in bool_flags {
        if let Some(v) = value {
            if get(config) != v {
                put(config, v);
            }
            sources.insert(path.to_string(), Source::Flag(flag.to_string()));
        }
    }
    if flags.verbose > 0 {
        config.log.level = match flags.verbose {
            1 => crate::model::LogLevel::Info,
            2 => crate::model::LogLevel::Debug,
            _ => crate::model::LogLevel::Trace,
        };
        sources.insert("log.level".to_string(), Source::Flag("-v".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_env_parsing_matches_spec() {
        for v in ["1", "true", "yes", "on", "TRUE"] {
            assert!(parse_bool_env(v).unwrap(), "{v}");
        }
        for v in ["0", "false", "no", ""] {
            assert!(!parse_bool_env(v).unwrap(), "{v}");
        }
        assert!(parse_bool_env("maybe").is_err());
        assert!(parse_bool_env("2").is_err());
    }

    #[test]
    fn env_keys_table_covers_spec() {
        let vars: Vec<&str> = ENV_KEYS.iter().map(|(v, _, _)| *v).collect();
        for expected in [
            "CAIRN_MODEL",
            "CAIRN_MODE",
            "CAIRN_PROFILE",
            "CAIRN_OUTPUT",
            "CAIRN_LOG_LEVEL",
            "CAIRN_LOG_FILE",
            "CAIRN_TRACE",
            "CAIRN_OFFLINE",
            "CAIRN_QUIET",
            "CAIRN_SHELL",
            "CAIRN_MAX_ITERATIONS",
            "CAIRN_NO_UPDATE_CHECK",
            "CAIRN_SCREEN_READER",
            "CAIRN_VERIFY_COMMANDS",
            "CAIRN_CACHE_DIR",
            "CAIRN_DATA_DIR",
        ] {
            assert!(vars.contains(&expected), "missing env var {expected}");
        }
        for (v, path, _) in ENV_KEYS {
            assert!(!path.is_empty(), "{v} has no target key");
        }
    }

    #[test]
    fn env_overrides_defaults() {
        let mut env = BTreeMap::new();
        env.insert(
            "CAIRN_MODEL".to_string(),
            "ollama/qwen2.5-coder:14b".to_string(),
        );
        env.insert("CAIRN_MODE".to_string(), "plan".to_string());
        env.insert("CAIRN_MAX_ITERATIONS".to_string(), "7".to_string());
        env.insert("CAIRN_NO_UPDATE_CHECK".to_string(), "1".to_string());
        env.insert("CAIRN_TRACE".to_string(), "true".to_string());
        let opts = LoadOptions {
            env: Some(env),
            ..Default::default()
        };
        let loaded = load(&opts);
        assert_eq!(loaded.config.model, "ollama/qwen2.5-coder:14b");
        assert_eq!(loaded.config.mode, cairn_core::Mode::Plan);
        assert_eq!(loaded.config.auto.max_iterations, 7);
        assert!(!loaded.config.update.check);
        assert!(loaded.config.trace.enabled);
        assert_eq!(loaded.sources["model"], Source::Env("CAIRN_MODEL".into()));
        assert_eq!(
            loaded.sources["update.check"],
            Source::Env("CAIRN_NO_UPDATE_CHECK".into())
        );
        assert!(
            loaded.fatal_issues().is_empty(),
            "{:?}",
            loaded.fatal_issues()
        );
    }

    #[test]
    fn bad_env_value_reports_badenv() {
        let mut env = BTreeMap::new();
        env.insert("CAIRN_TRACE".to_string(), "yep".to_string());
        env.insert("CAIRN_MODE".to_string(), "turbo".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            ..Default::default()
        });
        let codes: Vec<&str> = loaded.issues.iter().map(|i| i.code).collect();
        assert!(codes.contains(&codes::CFG_BADENV), "{codes:?}");
        assert!(codes.contains(&codes::CFG_BADVALUE), "{codes:?}");
        assert!(!loaded.fatal_issues().is_empty(), "bad mode must be fatal");
    }

    /// T-CFG-010: precedence flags > env > explicit > project > parents > user > system > default.
    #[test]
    fn precedence_layers_win_in_order() {
        let tmp = std::env::temp_dir().join(format!("cairn-cfg-{}-prec", std::process::id()));
        let ws = tmp.join("ws");
        std::fs::create_dir_all(ws.join(".cairn")).unwrap();
        std::fs::create_dir_all(tmp.join(".cairn")).unwrap();
        std::fs::create_dir_all(tmp.join("cfg")).unwrap();
        // workspace layer (highest project layer)
        std::fs::write(
            ws.join(".cairn/config.toml"),
            "temperature = 0.9\nmode = \"auto\"\n",
        )
        .unwrap();
        // parent (farther) layer
        std::fs::write(
            tmp.join(".cairn/config.toml"),
            "temperature = 0.7\nui.frame_rate = 144\n",
        )
        .unwrap();
        // explicit --config (above project)
        let explicit = tmp.join("cfg/explicit.toml");
        std::fs::write(&explicit, "temperature = 0.6\n").unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "CAIRN_MODEL".to_string(),
            "ollama/qwen2.5-coder:14b".to_string(),
        );
        let opts = LoadOptions {
            cwd: ws.clone(),
            explicit_config: Some(explicit.clone()),
            env: Some(env),
            flags: FlagOverrides {
                mode: Some("plan".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let loaded = load(&opts);
        assert_eq!(loaded.workspace, ws);
        // rank 1: flag beats env/project
        assert_eq!(loaded.config.mode, cairn_core::Mode::Plan);
        assert_eq!(loaded.sources["mode"], Source::Flag("--mode".into()));
        // rank 2: env beats project
        assert_eq!(loaded.config.model, "ollama/qwen2.5-coder:14b");
        assert_eq!(loaded.sources["model"], Source::Env("CAIRN_MODEL".into()));
        // rank 3: explicit beats project root
        assert_eq!(loaded.config.temperature, 0.6);
        assert_eq!(loaded.sources["temperature"], Source::Explicit);
        // rank 5: parent layer supplies keys nobody else set
        assert_eq!(loaded.config.ui.frame_rate, 144);
        assert_eq!(loaded.sources["ui.frame_rate"], Source::Project);
        assert!(
            loaded.fatal_issues().is_empty(),
            "{:?}",
            loaded.fatal_issues()
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// REQ-CLI-006: effective view annotates every key with its layer.
    #[test]
    fn effective_view_annotates_sources() {
        let mut env = BTreeMap::new();
        env.insert("CAIRN_MODE".to_string(), "plan".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            ..Default::default()
        });
        let eff = crate::schema::effective(&loaded);
        let by_path: BTreeMap<&str, &crate::schema::EffectiveEntry> =
            eff.iter().map(|e| (e.path.as_str(), e)).collect();
        assert_eq!(by_path["mode"].source, "env");
        assert_eq!(by_path["mode"].detail.as_deref(), Some("CAIRN_MODE"));
        assert_eq!(by_path["temperature"].source, "default");
        assert_eq!(by_path["ui.theme"].source, "default");
        assert!(
            eff.iter().any(|e| e.path == "shell.env_allowlist"),
            "arrays are listed too"
        );
        assert!(eff.iter().all(|e| !e.source.is_empty()));
        assert!(
            eff.len() > 100,
            "every leaf key must be listed, got {}",
            eff.len()
        );
    }

    #[test]
    fn append_syntax_appends_and_dedups() {
        let tmp = std::env::temp_dir().join(format!("cairn-cfg-append-{}", std::process::id()));
        let ws = tmp.join("ws");
        std::fs::create_dir_all(ws.join(".cairn")).unwrap();
        std::fs::create_dir_all(tmp.join(".cairn")).unwrap();
        // lower layer: the parent project config
        std::fs::write(
            tmp.join(".cairn/config.toml"),
            "discovery.exclude = [\"node_modules/**\", \"dist/**\"]\n",
        )
        .unwrap();
        // higher layer: the workspace project config appends to it
        std::fs::write(
            ws.join(".cairn/config.toml"),
            "discovery.exclude += [\"docs/**\", \"node_modules/**\"]\n",
        )
        .unwrap();

        let opts = LoadOptions {
            cwd: ws.clone(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        };
        let loaded = load(&opts);
        assert!(
            loaded.issues.iter().all(|i| !i.fatal),
            "{:?}",
            loaded.issues.iter().map(Issue::render).collect::<Vec<_>>()
        );
        assert_eq!(
            loaded.config.discovery.exclude,
            vec!["node_modules/**", "dist/**", "docs/**"],
            "append + dedup, order preserved"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// T-CFG-011: every row of the §11.5 merge-semantics table, exercised
    /// through the real layering (ancestor project config = lower layer,
    /// workspace project config = higher layer).
    #[test]
    fn t_cfg_011_merge_semantics_table() {
        let tmp = std::env::temp_dir().join(format!("cairn-cfg-merge-{}", std::process::id()));
        let ws = tmp.join("ws");
        std::fs::create_dir_all(ws.join(".cairn")).unwrap();
        std::fs::create_dir_all(tmp.join(".cairn")).unwrap();

        // Lower layer.
        std::fs::write(
            tmp.join(".cairn/config.toml"),
            concat!(
                "mode = \"auto\"\n",
                "temperature = 0.9\n",
                "discovery.include = [\"src/**\"]\n",
                "discovery.exclude = [\"node_modules/**\"]\n",
                "shell.env_allowlist = [\"GIT_*\", \"LANG\"]\n",
                "\n[ui]\n",
                "theme = \"cairn-light\"\n",
                "animation = \"on\"\n",
                "\n[[mcp.servers]]\n",
                "name = \"lower\"\n",
                "transport = \"stdio\"\n",
                "command = \"echo\"\n",
                "\n[[mcp.servers]]\n",
                "name = \"also-lower\"\n",
                "transport = \"stdio\"\n",
                "command = \"true\"\n",
            ),
        )
        .unwrap();

        // Higher layer.
        std::fs::write(
            ws.join(".cairn/config.toml"),
            concat!(
                "mode = \"plan\"\n",
                "discovery.include = [\"crates/**\"]\n",
                "discovery.exclude += [\"docs/**\", \"node_modules/**\"]\n",
                "shell.env_allowlist = [\"CAIRN_*\"]\n",
                "\n[ui]\n",
                "animation = \"off\"\n",
                "\n[[mcp.servers]]\n",
                "name = \"upper\"\n",
                "transport = \"stdio\"\n",
                "command = \"echo\"\n",
            ),
        )
        .unwrap();

        let loaded = load(&LoadOptions {
            cwd: ws.clone(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        let errs: Vec<String> = loaded
            .fatal_issues()
            .into_iter()
            .map(Issue::render)
            .collect();
        assert!(errs.is_empty(), "{errs:#?}");

        // | Scalar | higher layer replaces |
        assert_eq!(
            loaded.config.mode,
            cairn_core::Mode::Plan,
            "scalar replaced"
        );
        assert_eq!(loaded.config.temperature, 0.9, "absent scalar inherited");
        assert_eq!(loaded.sources["mode"], Source::Project);
        assert_eq!(loaded.sources["temperature"], Source::Project);

        // | Map / TOML table | key-wise recursive merge; absent keys inherit |
        assert_eq!(
            loaded.config.ui.theme.as_str(),
            "cairn-light",
            "absent key inherited"
        );
        assert_eq!(
            loaded.config.ui.animation,
            crate::model::Animation::Off,
            "present key replaced"
        );
        assert_eq!(loaded.sources["ui.animation"], Source::Project);

        // | Array of tables | replace entirely if the layer defines any |
        let names: Vec<&str> = loaded
            .config
            .mcp
            .servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["upper"],
            "project hooks/servers hide the ancestor's"
        );

        // | Array of scalars | plain assignment replaces … |
        assert_eq!(
            loaded.config.discovery.include,
            ["crates/**"],
            "plain list assignment replaces"
        );
        assert_eq!(
            loaded.config.shell.env_allowlist,
            ["CAIRN_*"],
            "plain list assignment replaces"
        );

        // | … `key += [...]` | append and dedup (order preserved) |
        assert_eq!(
            loaded.config.discovery.exclude,
            ["node_modules/**", "docs/**"],
            "append + dedup, order preserved"
        );
        assert_eq!(loaded.sources["discovery.exclude"], Source::Project);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// REQ-CLI-005: an invalid higher-layer value falls back, with a warning.
    #[test]
    fn invalid_higher_layer_value_falls_back() {
        let tmp = std::env::temp_dir().join(format!("cairn-cfg-fb-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".cairn")).unwrap();
        std::fs::write(
            tmp.join(".cairn/config.toml"),
            "temperature = 9.0\nmode = \"turbo\"\n",
        )
        .unwrap();

        let loaded = load(&LoadOptions {
            cwd: tmp.clone(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        assert_eq!(loaded.config.temperature, 0.2, "fell back to default");
        assert_eq!(
            loaded.config.mode,
            cairn_core::Mode::Build,
            "fell back to default"
        );
        let fallbacks: Vec<&Issue> = loaded
            .issues
            .iter()
            .filter(|i| i.code == codes::CFG_FALLBACK)
            .collect();
        assert!(
            fallbacks.len() >= 2,
            "expected fallback warnings: {fallbacks:?}"
        );
        assert!(fallbacks.iter().any(|i| i.message.contains("temperature")));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unknown_key_is_rejected_with_location() {
        let tmp = std::env::temp_dir().join(format!("cairn-cfg-unk-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join(".cairn")).unwrap();
        std::fs::write(
            tmp.join(".cairn/config.toml"),
            "mode = \"plan\"\ntempratur = 1.0\n",
        )
        .unwrap();

        let loaded = load(&LoadOptions {
            cwd: tmp.clone(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        let unknown: Vec<&Issue> = loaded
            .issues
            .iter()
            .filter(|i| i.code == codes::CFG_UNKNOWN)
            .collect();
        assert_eq!(unknown.len(), 1, "{:?}", loaded.issues);
        assert_eq!(
            unknown[0].line,
            Some(2),
            "span must point at line 2: {:?}",
            unknown[0]
        );
        assert!(loaded
            .fatal_issues()
            .iter()
            .any(|i| i.code == codes::CFG_UNKNOWN));

        // --allow-unknown-keys downgrades to a fallback warning
        let loaded2 = load(&LoadOptions {
            cwd: tmp.clone(),
            env: Some(BTreeMap::new()),
            allow_unknown_keys: true,
            ..Default::default()
        });
        assert!(
            !loaded2
                .fatal_issues()
                .iter()
                .any(|i| i.code == codes::CFG_UNKNOWN),
            "{:?}",
            loaded2.issues
        );
        assert_eq!(
            loaded2.config.mode,
            cairn_core::Mode::Plan,
            "valid keys still load"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_explicit_config_is_reported() {
        let loaded = load(&LoadOptions {
            explicit_config: Some(PathBuf::from("/nonexistent/cairn.toml")),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        assert!(loaded
            .issues
            .iter()
            .any(|i| i.code == codes::FS_NOTFOUND && i.fatal));
    }

    /// T-CFG-030: `CAIRN_HOME` redirects every path.
    #[test]
    fn ca_irn_home_redirects_paths_req_cli_007() {
        let root = std::env::temp_dir().join(format!("cairn-home-{}", std::process::id()));
        let mut env = BTreeMap::new();
        env.insert("CAIRN_HOME".to_string(), root.to_string_lossy().to_string());
        env.insert("HOME".to_string(), "/home/definitely-not-real".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            ..Default::default()
        });
        assert_eq!(loaded.paths.config_home, root.join("config"));
        assert_eq!(loaded.paths.data_home, root.join("data"));
        assert_eq!(loaded.paths.cache_home, root.join("cache"));
        assert!(loaded.paths.sessions_dir().starts_with(&root));
    }

    #[test]
    fn flag_overrides_beat_env() {
        let mut env = BTreeMap::new();
        env.insert("CAIRN_MODE".to_string(), "auto".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            flags: FlagOverrides {
                mode: Some("plan".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(loaded.config.mode, cairn_core::Mode::Plan);
        assert_eq!(loaded.sources["mode"], Source::Flag("--mode".into()));
    }
}
