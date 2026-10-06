//! Shell safety (SPEC §9.3): what a command line would run, and whether any
//! of it is something the policy must see.
//!
//! [`analyze`] parses with tree-sitter-bash (through `cairn-parse`), finds
//! every command that would execute — however deeply nested — and judges each
//! against the denylist, the chaining and redirection rules, and the
//! boundary. [`evaluate`] then asks the permission policy about each leaf and
//! keeps the strictest answer, so `ls && rm -rf build` is never judged by its
//! first word.

pub mod interactive;
pub mod jobs;
pub mod proc;
mod rules;

use std::path::Path;

use cairn_core::error::codes;
use cairn_core::Mode;
use cairn_parse::shell::{parse, Command, Script, Word};
use cairn_perm::{Decision, PermissionPolicy, PermissionRequest, ShellGate};

use self::rules::{Exec, Finding};
use crate::paths::Boundary;
use crate::types::Access;

/// What analysis needs to know about where the command runs.
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    pub boundary: &'a Boundary,
    pub cwd: &'a Path,
    /// The user's home directory as text, for `~` and `$HOME`.
    pub home: Option<&'a str>,
}

/// A boundary violation found in a redirection or argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsViolation {
    pub code: &'static str,
    pub message: String,
}

/// One command that would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// What rules are matched against. A leaf that redirects output to a
    /// file starts with `[write] `, so no read-only pattern can claim it.
    pub text: String,
    /// The denylist hit, as `#n: reason`.
    pub deny: Option<String>,
    pub gate: ShellGate,
    pub gate_reason: Option<String>,
    pub network: bool,
}

/// The result of reading a command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analysis {
    pub leaves: Vec<Leaf>,
    pub parse_failed: bool,
    pub fs: Option<FsViolation>,
    /// More than one command, builtins included: `a && b`.
    pub chain: bool,
}

impl Analysis {
    /// The commands that would run, as the user would recognise them.
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        self.leaves.iter().map(|l| l.text.as_str()).collect()
    }
}

/// How deep `sh -c`, `eval` and wrappers may nest before the rest is left
/// unread (and the command asks).
const MAX_DEPTH: usize = 8;

/// Commands that only change the shell's own state.
const NOOPS: [&str; 10] = [
    "cd", "true", "false", ":", "pushd", "popd", "export", "set", "test", "[",
];

/// Parse and judge `command`.
#[must_use]
pub fn analyze(command: &str, ctx: &Ctx<'_>) -> Analysis {
    let mut run = Run {
        ctx,
        out: Analysis::default(),
    };
    let script = parse(command);
    run.out.chain = script.commands.len() > 1;
    run.scan(&script, 0);
    run.text_checks(command);
    if script.has_error {
        run.out.parse_failed = true;
        run.out.leaves.push(Leaf {
            text: command.to_string(),
            deny: None,
            gate: ShellGate::AskAlways,
            gate_reason: Some("the command could not be parsed".to_string()),
            network: false,
        });
    }
    run.out
}

struct Run<'a, 'c> {
    ctx: &'a Ctx<'c>,
    out: Analysis,
}

fn raw_join(name: &Word, args: &[Word]) -> String {
    std::iter::once(name.raw.as_str())
        .chain(args.iter().map(|w| w.raw.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_numeric_fd(target: &str) -> bool {
    target == "-" || (!target.is_empty() && target.chars().all(|c| c.is_ascii_digit()))
}

const QUIET_DEVICES: [&str; 6] = [
    "/dev/null",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/zero",
    "/dev/stdin",
];

impl Run<'_, '_> {
    fn violation(&mut self, code: &'static str, message: String) {
        if self.out.fs.is_none() {
            self.out.fs = Some(FsViolation { code, message });
        }
    }

    /// Checks on the raw text the grammar does not see as commands.
    fn text_checks(&mut self, command: &str) {
        let squashed: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
        if squashed.contains("unset HISTFILE")
            || squashed.contains("HISTFILE=/dev/null")
            || squashed.contains("HISTFILE=\"\"")
        {
            self.push_denied(command, 10, "disables shell history");
        }
    }

    fn push_denied(&mut self, text: &str, rule: u8, reason: &str) {
        self.out.leaves.push(Leaf {
            text: text.to_string(),
            deny: Some(format!("#{rule}: {reason}")),
            gate: ShellGate::None,
            gate_reason: None,
            network: false,
        });
    }

    fn scan(&mut self, script: &Script, depth: usize) {
        for cmd in &script.commands {
            self.redirects(cmd, depth);
            self.fork_bomb(cmd);
            let Some(name) = &cmd.name else {
                continue;
            };
            let mut execs = Vec::new();
            self.unwrap_into(name, &cmd.args, depth, &mut execs, cmd);
            let carried: Vec<Finding> = execs
                .iter()
                .filter(|e| e.transparent)
                .flat_map(|e| rules::check(e, self.ctx))
                .collect();
            for exec in execs.iter().filter(|e| !e.transparent) {
                self.judge(script, cmd, exec, &carried, depth);
            }
        }
    }

    /// Resolve wrappers (`env`, `nohup`, `xargs`, `sh -c`, …) into the
    /// commands they run, pushing each, wrapper included.
    fn unwrap_into(
        &mut self,
        name: &Word,
        args: &[Word],
        depth: usize,
        out: &mut Vec<Exec>,
        cmd: &Command,
    ) {
        let (base, local) = classify(&name.text);
        let exec = Exec {
            name: base.clone(),
            raw_name: name.raw.clone(),
            local,
            args: args.to_vec(),
            substituted: false,
            transparent: false,
        };
        let me = out.len();
        out.push(exec);
        if local || depth >= MAX_DEPTH {
            return;
        }
        let inner = |from: usize| args.get(from..).filter(|a| !a.is_empty());
        match base.as_str() {
            "env" => {
                let mut i = 0;
                while let Some(a) = args.get(i) {
                    let t = a.text.as_str();
                    if matches!(t, "-u" | "-C" | "-S") {
                        i += 2;
                    } else if t.starts_with('-') || t.contains('=') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                self.unwrap_rest(inner(i), depth, out, cmd);
            }
            "command" | "builtin" | "exec" | "setsid" | "time" | "chronic" | "unbuffer" => {
                if args.iter().any(|a| matches!(a.text.as_str(), "-v" | "-V")) {
                    return;
                }
                let skip = args.iter().take_while(|a| a.text.starts_with('-')).count();
                self.unwrap_rest(inner(skip), depth, out, cmd);
            }
            "nohup" => self.unwrap_rest(inner(0), depth, out, cmd),
            "nice" | "ionice" | "stdbuf" => {
                let mut i = 0;
                while let Some(a) = args.get(i) {
                    if a.text.starts_with('-') {
                        // `-n 5` takes a separate value; `-n5` and `-5` do not.
                        let takes_value = a.text.len() == 2
                            && !a.text[1..].starts_with(|c: char| c.is_ascii_digit());
                        i += if takes_value { 2 } else { 1 };
                    } else {
                        break;
                    }
                }
                self.unwrap_rest(inner(i), depth, out, cmd);
            }
            "timeout" => {
                let skip = args.iter().take_while(|a| a.text.starts_with('-')).count();
                self.unwrap_rest(inner(skip + 1), depth, out, cmd);
            }
            "watch" => {
                let skip = args.iter().take_while(|a| a.text.starts_with('-')).count();
                self.unwrap_rest(inner(skip), depth, out, cmd);
            }
            "xargs" => {
                let mut i = 0;
                while let Some(a) = args.get(i) {
                    let t = a.text.as_str();
                    if matches!(t, "-I" | "-n" | "-P" | "-d" | "-L" | "-s" | "-E" | "-a") {
                        i += 2;
                    } else if t.starts_with('-') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                self.unwrap_rest(inner(i), depth, out, cmd);
            }
            "find" => self.find_exec(args, depth, out, cmd),
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash" | "fish" => {
                let c_at = args.iter().position(|a| {
                    a.text.starts_with('-') && !a.text.starts_with("--") && a.text.contains('c')
                });
                if let Some(text) = c_at.and_then(|i| args.get(i + 1)) {
                    self.nested_script(&text.text, depth, "sh -c");
                    out[me].transparent = true;
                }
            }
            "eval" => {
                if args.iter().any(|a| a.dynamic) {
                    self.out.leaves.push(Leaf {
                        text: raw_join(name, args),
                        deny: None,
                        gate: ShellGate::AskAlways,
                        gate_reason: Some("`eval` of a value only known at run time".to_string()),
                        network: false,
                    });
                } else {
                    let joined = args
                        .iter()
                        .map(|a| a.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.nested_script(&joined, depth, "eval");
                    out[me].transparent = true;
                }
            }
            _ => {}
        }
        if out.len() > me + 1
            && matches!(
                base.as_str(),
                "env"
                    | "command"
                    | "builtin"
                    | "exec"
                    | "setsid"
                    | "time"
                    | "chronic"
                    | "unbuffer"
                    | "nohup"
                    | "nice"
                    | "ionice"
                    | "stdbuf"
                    | "timeout"
                    | "watch"
                    | "xargs"
            )
        {
            out[me].transparent = true;
        }
    }

    fn unwrap_rest(
        &mut self,
        rest: Option<&[Word]>,
        depth: usize,
        out: &mut Vec<Exec>,
        cmd: &Command,
    ) {
        if let Some([first, tail @ ..]) = rest {
            self.unwrap_into(first, tail, depth + 1, out, cmd);
        }
    }

    fn nested_script(&mut self, text: &str, depth: usize, _via: &str) {
        let script = parse(text);
        if script.has_error {
            self.out.leaves.push(Leaf {
                text: text.to_string(),
                deny: None,
                gate: ShellGate::AskAlways,
                gate_reason: Some("a nested command could not be parsed".to_string()),
                network: false,
            });
        }
        self.scan(&script, depth + 1);
    }

    /// `find START… -exec CMD {} ;` runs CMD on what it finds.
    fn find_exec(&mut self, args: &[Word], depth: usize, out: &mut Vec<Exec>, cmd: &Command) {
        let starts: Vec<&Word> = args
            .iter()
            .take_while(|a| !a.text.starts_with('-') && !matches!(a.text.as_str(), "(" | "!"))
            .collect();
        let starts = if starts.is_empty() { vec![] } else { starts };
        let mut i = 0;
        while i < args.len() {
            let t = args[i].text.as_str();
            if t == "-delete" {
                for start in &starts {
                    out.push(Exec {
                        name: "rm".to_string(),
                        raw_name: "rm".to_string(),
                        local: false,
                        args: vec![word("-r"), (*start).clone()],
                        substituted: true,
                        transparent: false,
                    });
                }
            }
            if matches!(t, "-exec" | "-execdir" | "-ok" | "-okdir") {
                let end = args[i + 1..]
                    .iter()
                    .position(|a| matches!(a.text.as_str(), ";" | "+"))
                    .map_or(args.len(), |p| i + 1 + p);
                let body = &args[i + 1..end];
                let fills: Vec<Option<&Word>> = if starts.is_empty() {
                    vec![None]
                } else {
                    starts.iter().map(|s| Some(*s)).collect()
                };
                for fill in fills {
                    let words: Vec<Word> = body
                        .iter()
                        .map(|w| match (w.text.as_str(), fill) {
                            ("{}", Some(start)) => start.clone(),
                            _ => w.clone(),
                        })
                        .collect();
                    let mut inner = Vec::new();
                    if let Some((first, tail)) = words.split_first() {
                        self.unwrap_into(first, tail, depth + 1, &mut inner, cmd);
                    }
                    for mut e in inner {
                        e.substituted = fill.is_some();
                        out.push(e);
                    }
                }
                i = end;
            }
            i += 1;
        }
    }

    fn redirects(&mut self, cmd: &Command, depth: usize) {
        for redirect in &cmd.redirects {
            if let Some(body) = &redirect.heredoc {
                let feeds_shell = cmd.name_text().is_some_and(|n| {
                    matches!(
                        classify(n).0.as_str(),
                        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash"
                    )
                });
                if feeds_shell {
                    self.nested_script(body, depth, "heredoc");
                }
            }
            let Some(target) = &redirect.target else {
                continue;
            };
            let text = rules::expand_home(&target.text, self.ctx.home);
            if rules::dev_net(&text) {
                self.push_denied(&target.raw, 11, "opens a network socket through /dev/tcp");
                continue;
            }
            let writes = redirect.writes();
            if writes && matches!(redirect.op.as_str(), ">&" | "&>") && is_numeric_fd(&text) {
                continue;
            }
            if !writes && redirect.op != "<" {
                continue;
            }
            if QUIET_DEVICES.contains(&text.as_str()) || target.dynamic && text.contains('$') {
                continue;
            }
            let lowered = text.to_ascii_lowercase();
            if writes && (lowered.ends_with("/.bash_history") || lowered.ends_with("/.zsh_history"))
            {
                self.push_denied(&target.raw, 10, "overwrites shell history");
                continue;
            }
            let access = if writes { Access::Write } else { Access::Read };
            let Some(probe) = self.ctx.boundary.probe(&text, self.ctx.cwd, access) else {
                continue;
            };
            if probe.protected {
                self.violation(
                    codes::FS_PROTECTED,
                    format!(
                        "`{}` is a protected path and cannot be {}",
                        target.raw,
                        if writes { "written" } else { "read" }
                    ),
                );
            } else if writes && !probe.inside {
                self.violation(
                    codes::FS_ESCAPE,
                    format!(
                        "Path resolves outside the workspace (→ {}).",
                        probe.abs.display()
                    ),
                );
            }
        }
    }

    fn fork_bomb(&mut self, cmd: &Command) {
        let Some(function) = &cmd.in_function else {
            return;
        };
        let calls_itself = cmd.name_text() == Some(function.as_str());
        if calls_itself && (cmd.background || !cmd.fed_by.is_empty()) {
            let raw = cmd.name.as_ref().map_or(String::new(), |n| n.raw.clone());
            self.push_denied(&raw, 5, "a fork bomb");
        }
    }

    fn judge(
        &mut self,
        script: &Script,
        cmd: &Command,
        exec: &Exec,
        carried: &[Finding],
        depth: usize,
    ) {
        let name_word = Word {
            text: exec.raw_name.clone(),
            raw: exec.raw_name.clone(),
            dynamic: false,
        };
        let mut text = raw_join(&name_word, &exec.args);
        let mut deny = None;
        let mut gate = ShellGate::None;
        let mut gate_reason = None;
        for finding in rules::check(exec, self.ctx)
            .into_iter()
            .chain(carried.iter().cloned())
        {
            match finding {
                Finding::Deny(n, why) => deny = deny.or(Some(format!("#{n}: {why}"))),
                Finding::AskAlways(why) => {
                    gate = ShellGate::AskAlways;
                    gate_reason = Some(why);
                }
                Finding::AskUnlessUnsafe(why) => {
                    if gate != ShellGate::AskAlways {
                        gate = ShellGate::AskUnlessUnsafe;
                        gate_reason = Some(why);
                    }
                }
            }
        }
        // Arguments that name a protected path.
        if !exec.local {
            for (arg, access) in rules::path_args(exec) {
                let expanded = rules::expand_home(arg, self.ctx.home);
                if expanded.contains('$') || expanded.is_empty() {
                    continue;
                }
                if let Some(p) = self.ctx.boundary.probe(&expanded, self.ctx.cwd, access) {
                    if p.protected && access == Access::Write {
                        // §9.3.1 #9: a denylist hit (asks in auto-unsafe).
                        deny = deny.or(Some(format!("#9: writes to the protected path `{arg}`")));
                    } else if p.protected {
                        self.violation(
                            codes::FS_PROTECTED,
                            format!("`{arg}` is a protected path and cannot be read"),
                        );
                    }
                }
            }
        }
        // Pipelines and process substitution into an interpreter.
        if !exec.local && rules::is_interpreter(&exec.name) && depth == 0 {
            if let Some(f) = self.interpreter_feed(script, cmd, exec) {
                match f {
                    Finding::Deny(n, why) => deny = deny.or(Some(format!("#{n}: {why}"))),
                    Finding::AskAlways(why) => {
                        gate = ShellGate::AskAlways;
                        gate_reason = Some(why);
                    }
                    Finding::AskUnlessUnsafe(why) => {
                        if gate != ShellGate::AskAlways {
                            gate = ShellGate::AskUnlessUnsafe;
                            gate_reason = Some(why);
                        }
                    }
                }
            }
        }
        let writes_file = cmd.redirects.iter().any(|r| {
            r.writes()
                && !r.target.as_ref().is_some_and(|t| {
                    is_numeric_fd(&t.text) || QUIET_DEVICES.contains(&t.text.as_str())
                })
        });
        if writes_file || rules::writes_despite_name(exec) {
            text = format!("[write] {text}");
        }
        if NOOPS.contains(&exec.name.as_str())
            && !exec.local
            && deny.is_none()
            && gate == ShellGate::None
            && !writes_file
        {
            return;
        }
        self.out.leaves.push(Leaf {
            text,
            deny,
            gate,
            gate_reason,
            network: rules::is_network(&exec.name),
        });
    }

    /// §9.3.2: what feeds an interpreter decides whether it may run.
    fn interpreter_feed(&self, script: &Script, cmd: &Command, exec: &Exec) -> Option<Finding> {
        let positionals: Vec<&Word> = exec
            .args
            .iter()
            .filter(|a| !a.text.starts_with('-'))
            .collect();
        let python = exec.name.starts_with("python");
        let code_flag = exec.args.iter().any(|a| {
            let t = a.text.as_str();
            if python {
                matches!(t, "-c" | "-m")
            } else {
                t.len() > 1
                    && t.starts_with('-')
                    && !t.starts_with("--")
                    && t.chars().skip(1).any(|c| matches!(c, 'c' | 'e' | 'E'))
            }
        });
        let reads_stdin = !code_flag
            && (positionals.is_empty()
                || positionals.first().is_some_and(|w| w.text == "-")
                || exec.args.iter().any(|a| a.text == "-s"));
        let from_procsub = cmd.script_from_procsub;
        if !(reads_stdin && !cmd.fed_by.is_empty() || from_procsub) {
            return None;
        }
        let feeders: Vec<&Command> = cmd
            .fed_by
            .iter()
            .filter_map(|&f| script.commands.get(f))
            .collect();
        let names: Vec<String> = feeders
            .iter()
            .filter_map(|c| c.name_text().map(|n| classify(n).0))
            .collect();
        if names.iter().any(|n| rules::is_fetcher(n)) {
            return Some(Finding::Deny(
                7,
                "pipes downloaded code into an interpreter".to_string(),
            ));
        }
        if names.iter().any(|n| {
            matches!(n.as_str(), "base64" | "xxd" | "openssl")
                && feeders.iter().any(|c| {
                    c.args.iter().any(|a| {
                        matches!(
                            a.text.as_str(),
                            "-d" | "-D" | "--decode" | "-r" | "-a" | "-base64"
                        )
                    })
                })
        }) {
            return Some(Finding::AskAlways("runs decoded data as code".to_string()));
        }
        let local_cat = !feeders.is_empty()
            && feeders.iter().all(|c| {
                c.name_text() == Some("cat")
                    && !c.args.is_empty()
                    && c.args.iter().all(|a| {
                        a.text.starts_with('-')
                            || (!a.dynamic
                                && self
                                    .ctx
                                    .boundary
                                    .probe(&a.text, self.ctx.cwd, Access::Read)
                                    .is_some_and(|p| p.inside && !p.protected))
                    })
            });
        if local_cat {
            return Some(Finding::AskUnlessUnsafe(
                "runs a script from the workspace through an interpreter".to_string(),
            ));
        }
        Some(Finding::Deny(
            7,
            "pipes data into an interpreter".to_string(),
        ))
    }
}

fn word(text: &str) -> Word {
    Word {
        text: text.to_string(),
        raw: text.to_string(),
        dynamic: false,
    }
}

/// `(program name, written with a relative path)`.
fn classify(name: &str) -> (String, bool) {
    let trimmed = name.trim();
    if trimmed.contains('/') {
        if trimmed.starts_with('/') {
            let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
            return (strip_exe(base), false);
        }
        return (
            strip_exe(trimmed.rsplit('/').next().unwrap_or(trimmed)),
            true,
        );
    }
    (strip_exe(trimmed), false)
}

fn strip_exe(name: &str) -> String {
    name.strip_suffix(".exe").unwrap_or(name).to_string()
}

// ------------------------------------------------------------------ decide

/// The decision for a whole command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub decision: Decision,
    /// The error code a refusal carries when it is not plain `E-PERM-DENIED`.
    pub code: Option<&'static str>,
    pub message: Option<String>,
    /// The request that decided: what an approval is about and what
    /// "always" remembers.
    pub request: PermissionRequest,
}

fn severity(d: &Decision) -> u8 {
    match d {
        Decision::Allow { .. } => 0,
        Decision::Ask { .. } => 1,
        Decision::Deny { .. } => 2,
    }
}

/// Ask `policy` about every leaf of `analysis` and keep the strictest answer.
#[must_use]
pub fn evaluate(
    analysis: &Analysis,
    tool: &str,
    mode: Mode,
    policy: &dyn PermissionPolicy,
    whole: &str,
) -> Outcome {
    if let Some(fs) = &analysis.fs {
        let mut request = PermissionRequest::new(tool, mode).command(whole);
        request.protected_path = true;
        return Outcome {
            decision: policy.decide(&request),
            code: Some(fs.code),
            message: Some(fs.message.clone()),
            request,
        };
    }
    let chain = analysis.chain || analysis.leaves.len() > 1;
    let mut best: Option<Outcome> = None;
    let leaves: Vec<Leaf> = if analysis.leaves.is_empty() {
        // Only shell builtins: as harmless as `echo`.
        vec![Leaf {
            text: "echo".to_string(),
            deny: None,
            gate: ShellGate::None,
            gate_reason: None,
            network: false,
        }]
    } else {
        analysis.leaves.clone()
    };
    for leaf in &leaves {
        let mut request = PermissionRequest::new(tool, mode).command(&leaf.text);
        request.denylisted = leaf.deny.is_some();
        request.gate = leaf.gate;
        let decision = policy.decide(&request);
        let (code, message) = if let (Some(why), true) =
            (&leaf.deny, decision.effect() == cairn_perm::Effect::Deny)
        {
            if chain {
                (
                    Some(codes::PERM_CHAIN),
                    Some(format!(
                        "Command chains a denied operation: '{}'. Split the command. ({why})",
                        leaf.text
                    )),
                )
            } else {
                (None, Some(format!("`{}` is not allowed: {why}", leaf.text)))
            }
        } else if analysis.parse_failed && decision.effect() == cairn_perm::Effect::Deny {
            (
                Some(codes::PERM_BADPARSE),
                Some("the command could not be parsed, so it was not run".to_string()),
            )
        } else {
            (None, leaf.gate_reason.clone())
        };
        let outcome = Outcome {
            decision,
            code,
            message,
            request,
        };
        best = match best {
            Some(b) if severity(&b.decision) >= severity(&outcome.decision) => Some(b),
            _ => Some(outcome),
        };
    }
    best.expect("at least one leaf")
}
