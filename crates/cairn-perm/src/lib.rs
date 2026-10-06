//! `cairn-perm` — the permission engine (SPEC §9.1, §9.2, REQ-SAFE-001..005).
//!
//! * [`rule`] — the rule grammar: parsing, compiling, specificity.
//! * [`mod@evaluate`] — the pure `evaluate(rules, request) -> Decision`.
//! * [`defaults`] — the §9.2 table, from `assets/default_rules.json`.
//! * [`store`] — `permissions.json`: lenient load, atomic `0600` append.
//! * [`RulePolicy`] — the §3.4 `PermissionPolicy`, composing all of the above
//!   for one mode.
//!
//! Nothing here executes anything or touches a workspace: it answers "may
//! this happen?" and remembers the user's answers.

pub mod defaults;
pub mod evaluate;
pub mod rule;
pub mod store;

use std::path::PathBuf;
use std::sync::RwLock;

use cairn_core::Mode;

pub use evaluate::{evaluate, Decision, PermissionRequest};
pub use rule::{Effect, Rule, Scope, Skipped, Target, TargetKind};
pub use store::PermError;

/// §3.4's policy trait: decide, remember, reload.
pub trait PermissionPolicy: Send + Sync {
    /// Evaluate all rules for `req`.
    fn decide(&self, req: &PermissionRequest) -> Decision;
    /// Persist an "always" answer; `Scope::Session` stays in memory only.
    ///
    /// # Errors
    /// [`PermError`] when a file scope cannot be written.
    fn remember(
        &self,
        req: &PermissionRequest,
        effect: Effect,
        scope: Scope,
    ) -> Result<Rule, PermError>;
    /// Re-read the rule files.
    ///
    /// # Errors
    /// [`PermError`] when a file exists and cannot be understood.
    fn reload(&self) -> Result<(), PermError>;
}

/// Where a policy reads and writes rules.
#[derive(Debug, Clone, Default)]
pub struct PolicyFiles {
    /// `.cairn/permissions.json`.
    pub project: Option<PathBuf>,
    /// `~/.config/cairn/permissions.json`.
    pub user: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct State {
    user: Vec<rule::CompiledRule>,
    project: Vec<rule::CompiledRule>,
    session: Vec<rule::CompiledRule>,
    skipped: Vec<Skipped>,
}

#[derive(Debug)]
/// The rule-based policy for one mode: built-in defaults, then the user and
/// project files, then this session's answers.
pub struct RulePolicy {
    mode: Mode,
    files: PolicyFiles,
    home: Option<String>,
    case_insensitive: bool,
    defaults: Vec<rule::CompiledRule>,
    state: RwLock<State>,
}

impl RulePolicy {
    /// Build a policy and load its files.
    ///
    /// # Errors
    /// [`PermError`] when a file exists and cannot be understood.
    pub fn new(
        mode: Mode,
        files: PolicyFiles,
        home: Option<String>,
        case_insensitive: bool,
    ) -> Result<Self, PermError> {
        let policy = Self {
            mode,
            files,
            home,
            case_insensitive,
            defaults: defaults::defaults_for(mode, case_insensitive),
            state: RwLock::new(State::default()),
        };
        policy.reload()?;
        Ok(policy)
    }

    /// Rules the last load ignored, with reasons (`W-PERM-BADREGEX`, ...).
    #[must_use]
    pub fn skipped(&self) -> Vec<Skipped> {
        self.state.read().expect("policy state").skipped.clone()
    }

    /// The mode this policy decides for.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
}

impl PermissionPolicy for RulePolicy {
    fn decide(&self, req: &PermissionRequest) -> Decision {
        let state = self.state.read().expect("policy state");
        // Array order is the final tie-break (§9.1 rule 4): defaults first,
        // then user, project, session — later layers are "later rules".
        let all: Vec<rule::CompiledRule> = self
            .defaults
            .iter()
            .chain(&state.user)
            .chain(&state.project)
            .chain(&state.session)
            .cloned()
            .collect();
        evaluate(&all, req, self.home.as_deref())
    }

    fn remember(
        &self,
        req: &PermissionRequest,
        effect: Effect,
        scope: Scope,
    ) -> Result<Rule, PermError> {
        let (target, action) = match (&req.command, &req.path) {
            (Some(command), _) => (
                Some(Target {
                    kind: TargetKind::CommandPrefix,
                    value: command.clone(),
                }),
                req.tool.clone(),
            ),
            (None, Some(path)) => (
                Some(Target {
                    kind: TargetKind::PathGlob,
                    value: path.clone(),
                }),
                req.tool.clone(),
            ),
            _ => (None, req.tool.clone()),
        };
        let summary = req.command.as_deref().or(req.path.as_deref()).unwrap_or("");
        let note = format!(
            "{} {}: {summary} from {}",
            effect.as_str(),
            req.tool,
            chrono::Utc::now().format("%Y-%m-%d")
        );
        let rule = Rule {
            id: String::new(),
            effect,
            action,
            target,
            except: None,
            scope,
            created_at: None,
            note: Some(note.chars().take(200).collect()),
        };
        let stored = match scope {
            Scope::Session | Scope::Default => {
                let mut stored = rule;
                stored.id = format!(
                    "s{}",
                    self.state.read().expect("policy state").session.len() + 1
                );
                stored
            }
            Scope::Project | Scope::User => {
                let path = match scope {
                    Scope::Project => self.files.project.clone(),
                    _ => self.files.user.clone(),
                };
                let path = path.ok_or_else(|| PermError::Write {
                    path: format!("{scope:?} permissions file"),
                    source: std::io::Error::other("no file configured for this scope"),
                })?;
                store::append(&path, rule)?
            }
        };
        let compiled =
            rule::compile(stored.clone(), self.case_insensitive).map_err(|(_, why)| {
                PermError::Parse {
                    path: "new rule".to_string(),
                    reason: why,
                }
            })?;
        let mut state = self.state.write().expect("policy state");
        match scope {
            Scope::Project => state.project.push(compiled),
            Scope::User => state.user.push(compiled),
            _ => state.session.push(compiled),
        }
        Ok(stored)
    }

    fn reload(&self) -> Result<(), PermError> {
        let mut fresh = State::default();
        for (path, scope) in [
            (&self.files.user, Scope::User),
            (&self.files.project, Scope::Project),
        ] {
            let Some(path) = path else { continue };
            let loaded = store::load(path, scope, self.case_insensitive)?;
            fresh.skipped.extend(loaded.skipped);
            match scope {
                Scope::User => fresh.user = loaded.rules,
                _ => fresh.project = loaded.rules,
            }
        }
        let mut state = self.state.write().expect("policy state");
        // A reload re-reads files; this session's own answers survive it.
        fresh.session = std::mem::take(&mut state.session);
        *state = fresh;
        Ok(())
    }
}
