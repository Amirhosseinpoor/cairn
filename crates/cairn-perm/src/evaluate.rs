//! The evaluator: a pure function from rules and a request to a decision
//! (SPEC §9.1, REQ-SAFE-001). No I/O, no clock, no globals — everything it
//! needs is in its arguments, which is what lets a 200-case matrix pin it.

use cairn_core::Mode;

use crate::rule::{normalize_command, CompiledRule, Effect, HostPattern, Matcher, Scope};

/// One thing a tool wants to do, described the way rules talk about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// The tool's name (`edit_file`).
    pub tool: String,
    pub mode: Mode,
    /// Workspace-relative POSIX path, for path-bearing tools.
    pub path: Option<String>,
    /// The shell command, for `bash`-like tools.
    pub command: Option<String>,
    /// The URL, for `web_fetch`.
    pub url: Option<String>,
    /// Set by the boundary step (§9.4): this path is protected in every mode.
    pub protected_path: bool,
    /// Set by shell analysis (§9.3.1): a denylisted construct, at any depth.
    pub denylisted: bool,
}

impl PermissionRequest {
    #[must_use]
    pub fn new(tool: &str, mode: Mode) -> Self {
        Self {
            tool: tool.to_string(),
            mode,
            path: None,
            command: None,
            url: None,
            protected_path: false,
            denylisted: false,
        }
    }

    #[must_use]
    pub fn path(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }

    #[must_use]
    pub fn command(mut self, command: &str) -> Self {
        self.command = Some(command.to_string());
        self
    }

    #[must_use]
    pub fn url(mut self, url: &str) -> Self {
        self.url = Some(url.to_string());
        self
    }
}

/// The answer. `rule_id` names the rule that decided, or `"none"` when no
/// rule matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { rule_id: String },
    Ask { rule_id: String, reason: String },
    Deny { rule_id: String, reason: String },
}

impl Decision {
    #[must_use]
    pub const fn effect(&self) -> Effect {
        match self {
            Self::Allow { .. } => Effect::Allow,
            Self::Ask { .. } => Effect::Ask,
            Self::Deny { .. } => Effect::Deny,
        }
    }

    #[must_use]
    pub fn rule_id(&self) -> &str {
        match self {
            Self::Allow { rule_id } | Self::Ask { rule_id, .. } | Self::Deny { rule_id, .. } => {
                rule_id
            }
        }
    }
}

/// Split a URL into `(scheme, lowercase host)` without a URL crate: rules
/// only ever look at these two.
fn scheme_and_host(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| (scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

fn matcher_hits(matcher: &Matcher, req: &PermissionRequest, home: Option<&str>) -> bool {
    match matcher {
        Matcher::Any => true,
        Matcher::Path(glob) => req
            .path
            .as_deref()
            .is_some_and(|path| glob.is_match(path.trim_start_matches("./"))),
        Matcher::Prefix { words, open } => req.command.as_deref().is_some_and(|command| {
            let normalized = normalize_command(command, home);
            let tokens: Vec<&str> = normalized.split(' ').filter(|t| !t.is_empty()).collect();
            if tokens.len() < words.len() {
                return false;
            }
            let leading_equal = words.iter().zip(&tokens).all(|(w, t)| w == t);
            // Word boundary: `git *` never matches `gitx` because whole
            // tokens are compared. Without a trailing `*` the pattern must
            // be the whole command.
            leading_equal && (*open || tokens.len() == words.len())
        }),
        Matcher::Regex(regex) => req
            .command
            .as_deref()
            .is_some_and(|command| regex.is_match(&normalize_command(command, home))),
        Matcher::Host { https_only, host } => {
            let Some((scheme, found)) = req.url.as_deref().and_then(scheme_and_host) else {
                return false;
            };
            if *https_only && scheme != "https" {
                return false;
            }
            match host {
                HostPattern::Any => true,
                HostPattern::Exact(exact) => &found == exact,
                HostPattern::Suffix(suffix) => {
                    found == *suffix || found.ends_with(&format!(".{suffix}"))
                }
            }
        }
    }
}

/// Whether `rule` applies to `req`.
fn applies(rule: &CompiledRule, req: &PermissionRequest, home: Option<&str>) -> bool {
    rule.action.matches(&req.tool)
        && matcher_hits(&rule.target, req, home)
        && !rule
            .except
            .as_ref()
            .is_some_and(|except| matcher_hits(except, req, home))
}

/// §9.1's precedence as one sortable key: effect first (deny > ask > allow),
/// then specificity, then scope (project over user over default), then the
/// later rule in the array.
fn precedence(rule: &CompiledRule, index: usize) -> (Effect, u8, Scope, usize) {
    (rule.rule.effect, rule.specificity, rule.rule.scope, index)
}

/// The fallback when no rule matches: nothing may be assumed allowed, so
/// `plan` denies and the other modes ask. `auto-unsafe` is the user's own
/// bypass and allows.
fn no_match(req: &PermissionRequest) -> Decision {
    let reason = format!("no rule covers `{}`", req.tool);
    match req.mode {
        Mode::Plan => Decision::Deny {
            rule_id: "none".to_string(),
            reason,
        },
        Mode::Build | Mode::Auto => Decision::Ask {
            rule_id: "none".to_string(),
            reason,
        },
        Mode::AutoUnsafe => Decision::Allow {
            rule_id: "none".to_string(),
        },
    }
}

/// Decide `req` against `rules`.
///
/// Two request flags are decided before any rule is read, because no rule a
/// person or a model can write may undo them: a protected path is denied in
/// every mode including `auto-unsafe` (D12), and a denylisted command is
/// denied everywhere except `auto-unsafe`, where it asks (D8).
#[must_use]
pub fn evaluate(rules: &[CompiledRule], req: &PermissionRequest, home: Option<&str>) -> Decision {
    if req.protected_path {
        return Decision::Deny {
            rule_id: "D12".to_string(),
            reason: "protected path".to_string(),
        };
    }
    if req.denylisted {
        let rule_id = "D8".to_string();
        return if req.mode == Mode::AutoUnsafe {
            Decision::Ask {
                rule_id,
                reason: "denylisted command".to_string(),
            }
        } else {
            Decision::Deny {
                rule_id,
                reason: "denylisted command".to_string(),
            }
        };
    }
    let matching: Vec<(usize, &CompiledRule)> = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| applies(rule, req, home))
        .collect();
    // Layering. §9.1 sorts by effect first, which taken alone would let a
    // built-in `ask` beat a user's "always allow" and make that answer
    // pointless. So a built-in rule yields to any rule a person wrote that
    // matches — except a built-in *deny*, which is a mode's floor (plan mode
    // never writes) and stays in the contest.
    let person_spoke = matching
        .iter()
        .any(|(_, rule)| rule.rule.scope != Scope::Default);
    let winner = matching
        .into_iter()
        .filter(|(_, rule)| {
            !person_spoke || rule.rule.scope != Scope::Default || rule.rule.effect == Effect::Deny
        })
        .max_by_key(|(index, rule)| precedence(rule, *index));
    let Some((_, rule)) = winner else {
        return no_match(req);
    };
    let rule_id = rule.rule.id.clone();
    let reason = rule
        .rule
        .note
        .clone()
        .unwrap_or_else(|| format!("rule {rule_id}"));
    match rule.rule.effect {
        Effect::Allow => Decision::Allow { rule_id },
        Effect::Ask => Decision::Ask { rule_id, reason },
        Effect::Deny => Decision::Deny { rule_id, reason },
    }
}
