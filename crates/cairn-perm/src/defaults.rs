//! The built-in rule set (SPEC §9.2), from `assets/default_rules.json`.
//!
//! The file is the table: one row per `D<n>`, one effect per mode. D8
//! (denylisted commands) and D12 (protected paths) are not rows because no
//! rule may relax them — the evaluator decides them from request flags.

use cairn_core::Mode;
use serde::Deserialize;

use crate::rule::{compile, CompiledRule, Effect, Rule, Scope, Target};

/// The embedded asset (REQ-SAFE-005).
pub const DEFAULT_RULES_JSON: &str = include_str!("../../../assets/default_rules.json");

#[derive(Debug, Deserialize)]
struct Effects {
    plan: Effect,
    build: Effect,
    auto: Effect,
    #[serde(rename = "auto-unsafe")]
    auto_unsafe: Effect,
}

impl Effects {
    fn for_mode(&self, mode: Mode) -> Effect {
        match mode {
            Mode::Plan => self.plan,
            Mode::Build => self.build,
            Mode::Auto => self.auto,
            Mode::AutoUnsafe => self.auto_unsafe,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Row {
    id: String,
    actions: Vec<String>,
    target: Target,
    #[serde(default)]
    except: Option<Target>,
    effects: Effects,
}

#[derive(Debug, Deserialize)]
struct Asset {
    ruleset_version: String,
    readonly_commands: String,
    rules: Vec<Row>,
}

fn asset() -> Asset {
    // Compiled into the binary and covered by a test: a parse failure here is
    // a build defect, not a runtime condition.
    serde_json::from_str(DEFAULT_RULES_JSON).expect("assets/default_rules.json is valid")
}

/// `ruleset_version`, recorded in the session header (REQ-SAFE-005).
#[must_use]
pub fn ruleset_version() -> String {
    asset().ruleset_version
}

/// The §9.2 table for one mode, compiled.
#[must_use]
pub fn defaults_for(mode: Mode, case_insensitive: bool) -> Vec<CompiledRule> {
    let asset = asset();
    let readonly = asset.readonly_commands.clone();
    let resolve = |mut target: Target| {
        if target.value == "$readonly" {
            target.value.clone_from(&readonly);
        }
        target
    };
    let mut out = Vec::new();
    for row in asset.rules {
        let effect = row.effects.for_mode(mode);
        for action in &row.actions {
            let rule = Rule {
                id: row.id.clone(),
                effect,
                action: action.clone(),
                target: Some(resolve(row.target.clone())),
                except: row.except.clone().map(resolve),
                scope: Scope::Default,
                created_at: None,
                note: Some(format!("built-in rule {}", row.id)),
            };
            // The asset is ours; a row that fails to compile is a defect the
            // tests below catch.
            out.push(compile(rule, case_insensitive).expect("default rule compiles"));
        }
    }
    out
}

/// The read-only shell set (§9.2) as a regex source, for callers that need
/// the same classification (`bash_background`, the approval summary).
#[must_use]
pub fn readonly_commands_regex() -> String {
    format!("^{}$", asset().readonly_commands)
}
