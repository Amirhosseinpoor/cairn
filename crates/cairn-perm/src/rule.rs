//! Permission rules (SPEC §9.1): the on-disk shape, and the compiled form the
//! evaluator matches against.
//!
//! A rule file is read leniently — one malformed rule never costs the others
//! — and every skipped rule says why (REQ-SAFE-002). Skipping can only make
//! the outcome *less* permissive: an ignored `allow` is one fewer allow, and
//! an ignored `deny` falls back to the defaults, which are already strict.

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// `allow` / `ask` / `deny`, ordered so that a higher value wins (§9.1 rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Where a rule came from. The order is the tie-break of §9.1 rule 3
/// (`project > user > default`); a session answer is the user's most recent
/// word and ranks with the project file, above everything the user wrote
/// earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Default,
    User,
    Project,
    Session,
}

/// `target.kind` (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    PathGlob,
    CommandPrefix,
    CommandRegex,
    UrlHost,
    Any,
}

/// A rule's `target` object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub kind: TargetKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
}

impl Target {
    #[must_use]
    pub fn any() -> Self {
        Self {
            kind: TargetKind::Any,
            value: String::new(),
        }
    }
}

/// One permission rule as stored (§9.1's schema).
///
/// `except` is the one addition: a target the rule must *not* match. The
/// built-in `bash` rule D5 ("any other command") needs it, because §9.1's
/// "deny beats ask" would otherwise let it override D4's read-only set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub effect: Effect,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub except: Option<Target>,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_scope() -> Scope {
    Scope::Project
}

/// A rule that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The rule's id when it had one.
    pub id: Option<String>,
    /// `W-PERM-BADREGEX` for a regex, `E-PERM-BADPARSE` for everything else.
    pub code: &'static str,
    pub reason: String,
}

/// Which tools an `action` string names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionMatcher {
    /// `tool:*` — every tool.
    All,
    /// An exact tool name (`edit_file`).
    Exact(String),
    /// `mcp__*` — a name prefix.
    Prefix(String),
    /// `write:<glob>` — the write tools.
    Write,
}

/// The write tools a `write:<glob>` action covers.
pub const WRITE_TOOLS: [&str; 3] = ["write_file", "edit_file", "multi_edit"];

impl ActionMatcher {
    #[must_use]
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(name) => name == tool,
            Self::Prefix(prefix) => tool.starts_with(prefix.as_str()),
            Self::Write => WRITE_TOOLS.contains(&tool),
        }
    }
}

/// A compiled `command_prefix` / `command_regex` / `path_glob` / `url_host`.
#[derive(Debug, Clone)]
pub enum Matcher {
    Any,
    Path(GlobMatcher),
    /// Pattern words; a trailing `*` word is `open` and matches any rest.
    Prefix {
        words: Vec<String>,
        open: bool,
    },
    Regex(Regex),
    Host {
        https_only: bool,
        host: HostPattern,
    },
}

/// `example.com` or `*.example.com` (or `*` for any host).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Any,
    Exact(String),
    Suffix(String),
}

/// A rule ready to match, with its specificity precomputed (§9.1 rule 2).
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub rule: Rule,
    pub action: ActionMatcher,
    /// A `bash:<prefix>` / `write:<glob>` shorthand carries its own target.
    pub target: Matcher,
    pub except: Option<Matcher>,
    pub specificity: u8,
}

/// Normalise a command lexically before prefix matching (§9.1): collapse
/// whitespace, strip a leading `./`, expand a leading `~`.
#[must_use]
pub fn normalize_command(command: &str, home: Option<&str>) -> String {
    let mut words: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    if let Some(first) = words.first_mut() {
        if let Some(rest) = first.strip_prefix("./") {
            *first = rest.to_string();
        }
    }
    for word in &mut words {
        if word == "~" {
            if let Some(home) = home {
                *word = home.to_string();
            }
        } else if let Some(rest) = word.strip_prefix("~/") {
            if let Some(home) = home {
                *word = format!("{}/{rest}", home.trim_end_matches('/'));
            }
        }
    }
    words.join(" ")
}

fn compile_target(
    target: &Target,
    case_insensitive: bool,
) -> Result<(Matcher, u8), (&'static str, String)> {
    let bad = |reason: String| ("E-PERM-BADPARSE", reason);
    match target.kind {
        TargetKind::Any => Ok((Matcher::Any, 1)),
        TargetKind::PathGlob => {
            let pattern = target.value.trim_start_matches('/');
            if pattern.is_empty() {
                return Err(bad("path_glob needs a value".to_string()));
            }
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .case_insensitive(case_insensitive)
                .build()
                .map_err(|e| bad(format!("bad glob `{}`: {e}", target.value)))?;
            let specificity = if pattern.contains("**") { 6 } else { 12 };
            Ok((Matcher::Path(glob.compile_matcher()), specificity))
        }
        TargetKind::CommandPrefix => {
            let mut words: Vec<String> = target
                .value
                .split_whitespace()
                .map(str::to_string)
                .collect();
            if words.is_empty() {
                return Err(bad("command_prefix needs a value".to_string()));
            }
            let open = words.last().is_some_and(|w| w == "*");
            if open {
                words.pop();
            }
            // §9.1: two or more literal words are more specific than one.
            let specificity = if words.len() >= 2 { 15 } else { 10 };
            Ok((Matcher::Prefix { words, open }, specificity))
        }
        TargetKind::CommandRegex => {
            let mut source = target.value.clone();
            if !source.starts_with('^') {
                source.insert(0, '^');
            }
            if !source.ends_with('$') {
                source.push('$');
            }
            let regex = Regex::new(&source).map_err(|e| {
                (
                    "W-PERM-BADREGEX",
                    format!("bad regex `{}`: {e}", target.value),
                )
            })?;
            Ok((Matcher::Regex(regex), 20))
        }
        TargetKind::UrlHost => {
            let mut value = target.value.trim();
            let https_only = value.starts_with("https://");
            value = value.trim_start_matches("https://");
            if value.is_empty() {
                return Err(bad("url_host needs a value".to_string()));
            }
            let host = if value == "*" {
                HostPattern::Any
            } else if let Some(suffix) = value.strip_prefix("*.") {
                HostPattern::Suffix(suffix.to_ascii_lowercase())
            } else {
                HostPattern::Exact(value.to_ascii_lowercase())
            };
            Ok((Matcher::Host { https_only, host }, 10))
        }
    }
}

fn compile_action(action: &str) -> Result<(ActionMatcher, Option<Target>), String> {
    let action = action.trim();
    if action.is_empty() {
        return Err("empty action".to_string());
    }
    if action == "tool:*" {
        return Ok((ActionMatcher::All, None));
    }
    if let Some(prefix) = action.strip_prefix("bash:") {
        if prefix.trim().is_empty() {
            return Err("`bash:` needs a command prefix".to_string());
        }
        return Ok((
            ActionMatcher::Exact("bash".to_string()),
            Some(Target {
                kind: TargetKind::CommandPrefix,
                value: prefix.trim().to_string(),
            }),
        ));
    }
    if let Some(glob) = action.strip_prefix("write:") {
        if glob.trim().is_empty() {
            return Err("`write:` needs a glob".to_string());
        }
        return Ok((
            ActionMatcher::Write,
            Some(Target {
                kind: TargetKind::PathGlob,
                value: glob.trim().to_string(),
            }),
        ));
    }
    if let Some(prefix) = action.strip_suffix('*') {
        if prefix.is_empty() || prefix.contains(':') {
            return Err(format!("unsupported action `{action}`"));
        }
        return Ok((ActionMatcher::Prefix(prefix.to_string()), None));
    }
    if action.contains(':') || action.contains(char::is_whitespace) {
        return Err(format!("unsupported action `{action}`"));
    }
    Ok((ActionMatcher::Exact(action.to_string()), None))
}

/// Compile one rule. The error is the reason it must be ignored.
///
/// # Errors
/// `(code, reason)` — `W-PERM-BADREGEX` for a regex that does not compile,
/// `E-PERM-BADPARSE` for anything else malformed.
pub fn compile(rule: Rule, case_insensitive: bool) -> Result<CompiledRule, (&'static str, String)> {
    let (action, shorthand) =
        compile_action(&rule.action).map_err(|reason| ("E-PERM-BADPARSE", reason))?;
    // An explicit `target` wins over the one a shorthand implies.
    let target = rule
        .target
        .clone()
        .or(shorthand)
        .unwrap_or_else(Target::any);
    let (matcher, specificity) = compile_target(&target, case_insensitive)?;
    let except = rule
        .except
        .as_ref()
        .map(|t| compile_target(t, case_insensitive).map(|(m, _)| m))
        .transpose()?;
    Ok(CompiledRule {
        rule,
        action,
        target: matcher,
        except,
        specificity,
    })
}

/// Parse a rule array leniently: good rules compile, bad ones are listed.
#[must_use]
pub fn compile_all(
    values: &[serde_json::Value],
    default_scope: Scope,
    case_insensitive: bool,
) -> (Vec<CompiledRule>, Vec<Skipped>) {
    let mut good = Vec::new();
    let mut skipped = Vec::new();
    for value in values {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let mut rule: Rule = match serde_json::from_value(value.clone()) {
            Ok(rule) => rule,
            Err(error) => {
                skipped.push(Skipped {
                    id,
                    code: "E-PERM-BADPARSE",
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if value.get("scope").is_none() {
            rule.scope = default_scope;
        }
        match compile(rule, case_insensitive) {
            Ok(compiled) => good.push(compiled),
            Err((code, reason)) => skipped.push(Skipped { id, code, reason }),
        }
    }
    (good, skipped)
}
