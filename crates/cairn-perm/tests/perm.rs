//! T-PERM-001..005 and the matcher edge cases of SPEC §9.1.

use cairn_core::Mode;
use cairn_perm::defaults::{defaults_for, ruleset_version, DEFAULT_RULES_JSON};
use cairn_perm::rule::{compile, compile_all, normalize_command, CompiledRule, Rule};
use cairn_perm::{
    evaluate, Decision, Effect, PermissionPolicy, PermissionRequest, PolicyFiles, RulePolicy,
    Scope, Target, TargetKind,
};

#[allow(
    clippy::unnecessary_wraps,
    reason = "call sites pass it where a rule takes Option<Target>"
)]
fn target(kind: TargetKind, value: &str) -> Option<Target> {
    Some(Target {
        kind,
        value: value.to_string(),
    })
}

fn rule(id: &str, effect: Effect, action: &str, t: Option<Target>, scope: Scope) -> CompiledRule {
    compile(
        Rule {
            id: id.to_string(),
            effect,
            action: action.to_string(),
            target: t,
            except: None,
            scope,
            created_at: None,
            note: None,
        },
        false,
    )
    .expect("rule compiles")
}

fn bash(command: &str, mode: Mode) -> PermissionRequest {
    PermissionRequest::new("bash", mode).command(command)
}

fn decide(rules: &[CompiledRule], req: &PermissionRequest) -> Decision {
    evaluate(rules, req, Some("/home/u"))
}

// ------------------------------------------------------------- matchers

#[test]
fn command_prefix_respects_word_boundaries() {
    let r = [rule(
        "r1",
        Effect::Allow,
        "bash",
        target(TargetKind::CommandPrefix, "git *"),
        Scope::Project,
    )];
    for (command, hit) in [
        ("git status", true),
        ("git -C x push", true),
        ("git", true),
        ("gitx status", false),
        ("sudo git status", false),
        ("  git   status  ", true),
        ("./git status", true),
    ] {
        let allowed = decide(&r, &bash(command, Mode::Build)).effect() == Effect::Allow;
        assert_eq!(allowed, hit, "`{command}`");
    }
}

#[test]
fn a_prefix_without_a_star_must_be_the_whole_command() {
    let r = [rule(
        "r1",
        Effect::Allow,
        "bash",
        target(TargetKind::CommandPrefix, "npm test"),
        Scope::Project,
    )];
    assert_eq!(
        decide(&r, &bash("npm test", Mode::Build)).effect(),
        Effect::Allow
    );
    assert_eq!(
        decide(&r, &bash("npm test --watch", Mode::Build)).effect(),
        Effect::Ask
    );
}

#[test]
fn a_lone_star_prefix_matches_every_command() {
    let r = [rule(
        "r1",
        Effect::Deny,
        "bash",
        target(TargetKind::CommandPrefix, "*"),
        Scope::Project,
    )];
    assert_eq!(
        decide(&r, &bash("anything at all", Mode::Auto)).effect(),
        Effect::Deny
    );
}

#[test]
fn tilde_expands_before_matching() {
    assert_eq!(
        normalize_command("cat ~/notes ~ x~y", Some("/home/u")),
        "cat /home/u/notes /home/u x~y"
    );
    assert_eq!(normalize_command("./run.sh  a", None), "run.sh a");
}

#[test]
fn path_globs_are_workspace_relative_and_star_stops_at_slash() {
    let r = [
        rule(
            "r1",
            Effect::Allow,
            "edit_file",
            target(TargetKind::PathGlob, "src/*.rs"),
            Scope::Project,
        ),
        rule(
            "r2",
            Effect::Allow,
            "edit_file",
            target(TargetKind::PathGlob, "/docs/**"),
            Scope::Project,
        ),
    ];
    let edit = |p: &str| PermissionRequest::new("edit_file", Mode::Build).path(p);
    assert_eq!(decide(&r, &edit("src/a.rs")).effect(), Effect::Allow);
    assert_eq!(
        decide(&r, &edit("src/deep/a.rs")).effect(),
        Effect::Ask,
        "`*` does not cross `/`"
    );
    assert_eq!(
        decide(&r, &edit("docs/a/b/c.md")).effect(),
        Effect::Allow,
        "`**` does"
    );
    assert_eq!(
        decide(&r, &edit("./docs/x.md")).effect(),
        Effect::Allow,
        "leading ./ is ignored"
    );
    assert_eq!(decide(&r, &edit("other.rs")).effect(), Effect::Ask);
}

#[test]
fn path_matching_is_case_sensitive_unless_the_volume_is_not() {
    let make = |ci| {
        compile(
            Rule {
                id: "r1".into(),
                effect: Effect::Deny,
                action: "write_file".into(),
                target: target(TargetKind::PathGlob, "Secrets/**"),
                except: None,
                scope: Scope::Project,
                created_at: None,
                note: None,
            },
            ci,
        )
        .expect("compiles")
    };
    let req = PermissionRequest::new("write_file", Mode::Build).path("secrets/key");
    assert_eq!(decide(&[make(false)], &req).effect(), Effect::Ask);
    assert_eq!(decide(&[make(true)], &req).effect(), Effect::Deny);
}

#[test]
fn url_hosts_match_exactly_by_suffix_and_by_scheme() {
    let r = [
        rule(
            "r1",
            Effect::Allow,
            "web_fetch",
            target(TargetKind::UrlHost, "docs.rs"),
            Scope::Project,
        ),
        rule(
            "r2",
            Effect::Allow,
            "web_fetch",
            target(TargetKind::UrlHost, "*.rust-lang.org"),
            Scope::Project,
        ),
        rule(
            "r3",
            Effect::Allow,
            "web_fetch",
            target(TargetKind::UrlHost, "https://*.github.io"),
            Scope::Project,
        ),
    ];
    let fetch = |u: &str| PermissionRequest::new("web_fetch", Mode::Build).url(u);
    assert_eq!(
        decide(&r, &fetch("https://docs.rs/x")).effect(),
        Effect::Allow
    );
    assert_eq!(
        decide(&r, &fetch("https://DOCS.rs:443/x?q=1")).effect(),
        Effect::Allow
    );
    assert_eq!(
        decide(&r, &fetch("https://evil.docs.rs/x")).effect(),
        Effect::Ask,
        "exact means exact"
    );
    assert_eq!(
        decide(&r, &fetch("https://www.rust-lang.org/")).effect(),
        Effect::Allow
    );
    assert_eq!(
        decide(&r, &fetch("https://rust-lang.org/")).effect(),
        Effect::Allow,
        "suffix includes the bare domain"
    );
    assert_eq!(
        decide(&r, &fetch("https://notrust-lang.org/")).effect(),
        Effect::Ask
    );
    assert_eq!(
        decide(&r, &fetch("https://a.github.io/")).effect(),
        Effect::Allow
    );
    assert_eq!(
        decide(&r, &fetch("http://a.github.io/")).effect(),
        Effect::Ask,
        "https-only rule"
    );
    assert_eq!(
        decide(&r, &fetch("https://user@docs.rs@evil.com/")).effect(),
        Effect::Ask,
        "userinfo is not the host"
    );
    assert_eq!(decide(&r, &fetch("not a url")).effect(), Effect::Ask);
}

#[test]
fn action_shorthands_name_their_tools() {
    let r = [
        rule("r1", Effect::Allow, "bash:cargo *", None, Scope::Project),
        rule("r2", Effect::Allow, "write:docs/**", None, Scope::Project),
        rule("r3", Effect::Deny, "mcp__*", None, Scope::Project),
    ];
    assert_eq!(
        decide(&r, &bash("cargo test", Mode::Build)).effect(),
        Effect::Allow
    );
    assert_eq!(decide(&r, &bash("make", Mode::Build)).effect(), Effect::Ask);
    for tool in ["write_file", "edit_file", "multi_edit"] {
        let req = PermissionRequest::new(tool, Mode::Build).path("docs/a.md");
        assert_eq!(decide(&r, &req).effect(), Effect::Allow, "{tool}");
    }
    let other = PermissionRequest::new("read_file", Mode::Build).path("docs/a.md");
    assert_eq!(
        decide(&r, &other).rule_id(),
        "none",
        "`write:` covers only write tools"
    );
    assert_eq!(
        decide(&r, &PermissionRequest::new("mcp__gh__issue", Mode::Auto)).effect(),
        Effect::Deny
    );
}

#[test]
fn tool_star_matches_every_tool() {
    let r = [rule("r1", Effect::Deny, "tool:*", None, Scope::User)];
    for tool in ["read_file", "bash", "mcp__x__y", "anything"] {
        assert_eq!(
            decide(&r, &PermissionRequest::new(tool, Mode::AutoUnsafe)).effect(),
            Effect::Deny
        );
    }
}

// ----------------------------------------------------------- precedence

/// Independent restatement of §9.1 used to check the evaluator over a large
/// generated matrix: effect, then specificity, then scope, then position —
/// with a person's rule shadowing a built-in `ask`/`allow`.
fn oracle(rules: &[(Effect, u8, Scope)]) -> Option<usize> {
    let person = rules.iter().any(|(_, _, scope)| *scope != Scope::Default);
    let mut best: Option<usize> = None;
    for (i, (effect, spec, scope)) in rules.iter().enumerate() {
        if person && *scope == Scope::Default && *effect != Effect::Deny {
            continue;
        }
        best = Some(match best {
            None => i,
            Some(b) => {
                let (be, bs, bsc) = rules[b];
                let candidate = (*effect as u8, *spec, *scope as u8, i);
                let current = (be as u8, bs, bsc as u8, b);
                if candidate > current {
                    i
                } else {
                    b
                }
            }
        });
    }
    best
}

/// Targets of every specificity the spec ranks, all matching `git status`
/// under mode `Build` so that only precedence decides.
fn spec_targets() -> Vec<(u8, TargetKind, &'static str)> {
    vec![
        (1, TargetKind::Any, ""),
        (10, TargetKind::CommandPrefix, "git *"),
        (15, TargetKind::CommandPrefix, "git status *"),
        (20, TargetKind::CommandRegex, "git .*"),
    ]
}

/// T-PERM-001: the exhaustive matrix over (effect × specificity × scope) for
/// every pair of rules, 1,296 cases, against the independent oracle.
#[test]
fn t_perm_001_matrix_over_effect_specificity_and_scope() {
    let effects = [Effect::Allow, Effect::Ask, Effect::Deny];
    let scopes = [Scope::Default, Scope::User, Scope::Project];
    let targets = spec_targets();
    let req = bash("git status", Mode::Build);
    let mut cases = 0;
    for ea in effects {
        for eb in effects {
            for (sa, ka, va) in &targets {
                for (sb, kb, vb) in &targets {
                    for sca in scopes {
                        for scb in scopes {
                            let a = rule("A", ea, "bash", target(*ka, va), sca);
                            let b = rule("B", eb, "bash", target(*kb, vb), scb);
                            let got = decide(&[a, b], &req);
                            let want = oracle(&[(ea, *sa, sca), (eb, *sb, scb)])
                                .map(|i| ["A", "B"][i])
                                .expect("something wins");
                            assert_eq!(
                                got.rule_id(),
                                want,
                                "A={ea:?}/{sa}/{sca:?} B={eb:?}/{sb}/{scb:?}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(cases >= 200, "{cases} cases");
    assert_eq!(cases, 1296);
}

/// The hand-written half of T-PERM-001: each §9.1 sentence as a case.
#[test]
fn t_perm_001_golden_cases() {
    let git = target(TargetKind::CommandPrefix, "git *");
    let any = None;
    // Deny > Ask > Allow, whatever the specificity.
    let r = [
        rule(
            "allow-regex",
            Effect::Allow,
            "bash",
            target(TargetKind::CommandRegex, "git status"),
            Scope::Project,
        ),
        rule("deny-any", Effect::Deny, "bash", any.clone(), Scope::User),
    ];
    assert_eq!(
        decide(&r, &bash("git status", Mode::Build)).rule_id(),
        "deny-any"
    );
    // Within one effect the more specific wins.
    let r = [
        rule("broad", Effect::Allow, "bash", git.clone(), Scope::Project),
        rule(
            "narrow",
            Effect::Allow,
            "bash",
            target(TargetKind::CommandPrefix, "git status *"),
            Scope::Project,
        ),
    ];
    assert_eq!(
        decide(&r, &bash("git status -s", Mode::Build)).rule_id(),
        "narrow"
    );
    // `path_glob` with no `**` (12) beats one with (6).
    let r = [
        rule(
            "deep",
            Effect::Allow,
            "edit_file",
            target(TargetKind::PathGlob, "src/**"),
            Scope::Project,
        ),
        rule(
            "flat",
            Effect::Allow,
            "edit_file",
            target(TargetKind::PathGlob, "src/*.rs"),
            Scope::Project,
        ),
    ];
    let edit = PermissionRequest::new("edit_file", Mode::Build).path("src/a.rs");
    assert_eq!(decide(&r, &edit).rule_id(), "flat");
    // Equal everything: project beats user beats default, then the later rule.
    let r = [
        rule("user", Effect::Allow, "bash", git.clone(), Scope::User),
        rule(
            "project",
            Effect::Allow,
            "bash",
            git.clone(),
            Scope::Project,
        ),
    ];
    assert_eq!(
        decide(&r, &bash("git log", Mode::Build)).rule_id(),
        "project"
    );
    let r = [
        rule("first", Effect::Allow, "bash", git.clone(), Scope::Project),
        rule("second", Effect::Allow, "bash", git, Scope::Project),
    ];
    assert_eq!(
        decide(&r, &bash("git log", Mode::Build)).rule_id(),
        "second"
    );
}

/// A person's allow outranks a built-in ask (so "always allow" does something)
/// but never a built-in deny (plan mode stays read-only).
#[test]
fn a_users_rule_beats_a_builtin_ask_but_not_a_builtin_deny() {
    let mut rules = defaults_for(Mode::Build, false);
    rules.push(rule(
        "r1",
        Effect::Allow,
        "edit_file",
        target(TargetKind::PathGlob, "src/**"),
        Scope::Project,
    ));
    let edit = PermissionRequest::new("edit_file", Mode::Build).path("src/a.rs");
    assert_eq!(
        decide(&rules, &edit),
        Decision::Allow {
            rule_id: "r1".into()
        }
    );
    let other = PermissionRequest::new("edit_file", Mode::Build).path("tests/a.rs");
    assert_eq!(
        decide(&rules, &other).rule_id(),
        "D3",
        "still asks elsewhere"
    );

    let mut plan = defaults_for(Mode::Plan, false);
    plan.push(rule(
        "r1",
        Effect::Allow,
        "edit_file",
        target(TargetKind::PathGlob, "**"),
        Scope::Project,
    ));
    let edit = PermissionRequest::new("edit_file", Mode::Plan).path("src/a.rs");
    assert_eq!(
        decide(&plan, &edit).effect(),
        Effect::Deny,
        "plan never writes"
    );

    let mut bash_plan = defaults_for(Mode::Plan, false);
    bash_plan.push(rule(
        "r2",
        Effect::Allow,
        "bash:make *",
        None,
        Scope::Project,
    ));
    assert_eq!(
        decide(&bash_plan, &bash("make all", Mode::Plan)).effect(),
        Effect::Deny
    );
}

/// Protected paths and denylisted commands are decided before any rule is
/// read: no allow rule can reach them (D12, D8).
#[test]
fn flags_are_decided_before_rules() {
    let allow_all = [rule("r1", Effect::Allow, "tool:*", None, Scope::Session)];
    for mode in Mode::ALL {
        let mut req = PermissionRequest::new("write_file", mode).path(".env");
        req.protected_path = true;
        assert_eq!(
            decide(&allow_all, &req),
            Decision::Deny {
                rule_id: "D12".into(),
                reason: "protected path".into()
            },
            "{mode:?}"
        );
    }
    for mode in [Mode::Plan, Mode::Build, Mode::Auto] {
        let mut req = bash("sudo rm x", mode);
        req.denylisted = true;
        assert_eq!(decide(&allow_all, &req).effect(), Effect::Deny, "{mode:?}");
    }
    let mut req = bash("sudo rm x", Mode::AutoUnsafe);
    req.denylisted = true;
    assert_eq!(
        decide(&allow_all, &req).effect(),
        Effect::Ask,
        "unsafe mode asks instead"
    );
}

/// With no rule at all the answer is never `Allow` — except in the mode whose
/// whole point is the bypass.
#[test]
fn an_empty_rule_set_is_fail_safe() {
    let req = |mode| PermissionRequest::new("mystery_tool", mode);
    assert_eq!(decide(&[], &req(Mode::Plan)).effect(), Effect::Deny);
    assert_eq!(decide(&[], &req(Mode::Build)).effect(), Effect::Ask);
    assert_eq!(decide(&[], &req(Mode::Auto)).effect(), Effect::Ask);
    assert_eq!(decide(&[], &req(Mode::AutoUnsafe)).effect(), Effect::Allow);
}

// ------------------------------------------------------------ T-PERM-002

/// T-PERM-002: bad rules are ignored, each with its reason, and the decision
/// falls back to *less* allow (REQ-SAFE-002).
#[test]
fn t_perm_002_malformed_rules_are_skipped_never_widened() {
    let values: Vec<serde_json::Value> = serde_json::from_str(
        r#"[
          {"id":"r1","effect":"allow","action":"bash","target":{"kind":"command_regex","value":"(unclosed"}},
          {"id":"r2","effect":"allow","action":""},
          {"id":"r3","effect":"sometimes","action":"bash"},
          {"id":"r4","action":"bash"},
          {"id":"r5","effect":"allow","action":"bash:"},
          {"id":"r6","effect":"allow","action":"weird thing"},
          {"id":"r7","effect":"allow","action":"edit_file","target":{"kind":"path_glob","value":"[unclosed"}},
          "not an object",
          {"id":"r8","effect":"allow","action":"bash:git *"}
        ]"#,
    )
    .expect("json");
    let (good, skipped) = compile_all(&values, Scope::Project, false);
    assert_eq!(good.len(), 1, "only r8 survives");
    assert_eq!(good[0].rule.id, "r8");
    assert_eq!(skipped.len(), 8);
    let regex = skipped
        .iter()
        .find(|s| s.id.as_deref() == Some("r1"))
        .expect("r1");
    assert_eq!(regex.code, "W-PERM-BADREGEX");
    assert!(
        skipped
            .iter()
            .filter(|s| s.code == "E-PERM-BADPARSE")
            .count()
            >= 6
    );

    // The ignored allow-regex did not become an allow-everything.
    assert_eq!(
        decide(&good, &bash("rm -rf x", Mode::Build)).effect(),
        Effect::Ask
    );
    assert_eq!(
        decide(&good, &bash("git status", Mode::Build)).effect(),
        Effect::Allow
    );
}

// ------------------------------------------------------------ T-PERM-003

/// T-PERM-003: the live defaults behave exactly as §9.2's table says, in each
/// of the four modes. The expectations below are the table, written out.
#[test]
fn t_perm_003_defaults_equal_the_spec_table() {
    use Effect::{Allow, Ask, Deny};
    type Case = (
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        [Effect; 4],
    );
    // (tool, path, command, [plan, build, auto, auto-unsafe])
    let cases: Vec<Case> = vec![
        (
            "read_file",
            Some("src/a.rs"),
            None,
            [Allow, Allow, Allow, Allow],
        ),
        ("list_dir", Some("."), None, [Allow, Allow, Allow, Allow]),
        ("glob", Some("."), None, [Allow, Allow, Allow, Allow]),
        ("grep", Some("."), None, [Allow, Allow, Allow, Allow]),
        ("git_status", None, None, [Allow, Allow, Allow, Allow]),
        ("git_diff", None, None, [Allow, Allow, Allow, Allow]),
        ("job_output", None, None, [Allow, Allow, Allow, Allow]),
        (
            "todo_write",
            Some(".cairn/todos.json"),
            None,
            [Allow, Allow, Allow, Allow],
        ),
        (
            "todo_write",
            Some("src/todos.json"),
            None,
            [Deny, Ask, Ask, Allow],
        ),
        (
            "write_file",
            Some("src/a.rs"),
            None,
            [Deny, Ask, Ask, Allow],
        ),
        ("edit_file", Some("src/a.rs"), None, [Deny, Ask, Ask, Allow]),
        (
            "multi_edit",
            Some("a/b/c.rs"),
            None,
            [Deny, Ask, Ask, Allow],
        ),
        ("bash", None, Some("ls -la"), [Ask, Ask, Allow, Allow]),
        ("bash", None, Some("git status"), [Ask, Ask, Allow, Allow]),
        (
            "bash",
            None,
            Some("git log --oneline"),
            [Ask, Ask, Allow, Allow],
        ),
        ("bash", None, Some("rg foo src"), [Ask, Ask, Allow, Allow]),
        (
            "bash",
            None,
            Some("cargo --version"),
            [Ask, Ask, Allow, Allow],
        ),
        (
            "bash",
            None,
            Some("curl -I https://x"),
            [Ask, Ask, Allow, Allow],
        ),
        ("bash", None, Some("rm -rf /tmp/x"), [Deny, Ask, Ask, Allow]),
        ("bash", None, Some("npm install"), [Deny, Ask, Ask, Allow]),
        ("bash", None, Some("git push"), [Deny, Ask, Ask, Allow]),
        ("bash", None, Some("lsof -i"), [Deny, Ask, Ask, Allow]),
        ("git_commit", None, None, [Deny, Ask, Ask, Allow]),
        ("subagent", None, None, [Ask, Ask, Ask, Allow]),
        (
            "mcp__github__list_issues",
            None,
            None,
            [Ask, Ask, Ask, Allow],
        ),
        (
            "bash_background",
            None,
            Some("tail -f log"),
            [Deny, Ask, Allow, Allow],
        ),
        (
            "bash_background",
            None,
            Some("cargo watch"),
            [Deny, Ask, Ask, Allow],
        ),
        ("ask_user", None, None, [Allow, Allow, Allow, Allow]),
    ];
    for (tool, path, command, expect) in cases {
        for (mode, want) in Mode::ALL.into_iter().zip(expect) {
            let mut req = PermissionRequest::new(tool, mode);
            req.path = path.map(str::to_string);
            req.command = command.map(str::to_string);
            let got = decide(&defaults_for(mode, false), &req);
            assert_eq!(
                got.effect(),
                want,
                "{tool} {path:?} {command:?} in {mode:?}: {got:?}"
            );
        }
    }
    // https fetches are D7 (allow in auto); plain http has no rule.
    let fetch = |mode, url: &str| {
        decide(
            &defaults_for(mode, false),
            &PermissionRequest::new("web_fetch", mode).url(url),
        )
        .effect()
    };
    assert_eq!(fetch(Mode::Auto, "https://docs.rs/x"), Allow);
    assert_eq!(fetch(Mode::Build, "https://docs.rs/x"), Ask);
    assert_eq!(fetch(Mode::Auto, "http://docs.rs/x"), Ask);
    assert_eq!(fetch(Mode::Plan, "https://docs.rs/x"), Ask);
}

/// The asset is a single versioned file that every row of parses and compiles.
#[test]
fn the_default_asset_parses_and_is_versioned() {
    assert!(!ruleset_version().is_empty());
    let doc: serde_json::Value = serde_json::from_str(DEFAULT_RULES_JSON).expect("JSON");
    let ids: Vec<&str> = doc["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    for want in [
        "D1", "D2", "D3", "D4", "D5", "D6", "D7", "D9", "D10", "D11", "D13",
    ] {
        assert!(ids.contains(&want), "{want} missing from {ids:?}");
    }
    for mode in Mode::ALL {
        assert!(!defaults_for(mode, false).is_empty());
    }
}

// ------------------------------------------------------------ T-PERM-004

/// Tiny deterministic generator: the property test must not depend on a
/// crate or on the clock.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.next()).expect("fits") % items.len()]
    }
}

/// T-PERM-004: over 2,000 random rule sets, the decision's effect is the
/// strongest effect among the rules that match — a deny is never outvoted.
#[test]
fn t_perm_004_deny_always_beats_allow() {
    let mut rng = Lcg(0x00C0_FFEE);
    let targets = [
        (TargetKind::Any, ""),
        (TargetKind::CommandPrefix, "git *"),
        (TargetKind::CommandPrefix, "git status *"),
        (TargetKind::CommandPrefix, "npm test"),
        (TargetKind::CommandRegex, "git .*"),
        (TargetKind::CommandRegex, "rm .*"),
    ];
    let effects = [Effect::Allow, Effect::Ask, Effect::Deny];
    let scopes = [Scope::Default, Scope::User, Scope::Project, Scope::Session];
    let commands = [
        "git status",
        "git status -s",
        "npm test",
        "rm -rf x",
        "ls",
        "git log",
    ];
    for case in 0..2000 {
        let count = 1 + usize::try_from(rng.next() % 6).expect("small");
        let rules: Vec<CompiledRule> = (0..count)
            .map(|i| {
                let (kind, value) = rng.pick(&targets);
                rule(
                    &format!("r{i}"),
                    rng.pick(&effects),
                    "bash",
                    target(kind, value),
                    rng.pick(&scopes),
                )
            })
            .collect();
        let command = rng.pick(&commands);
        let req = bash(command, Mode::Build);
        let got = decide(&rules, &req);
        let matching: Vec<&CompiledRule> = rules
            .iter()
            .filter(|r| decide(std::slice::from_ref(*r), &req).rule_id() != "none")
            .collect();
        if matching.is_empty() {
            assert_eq!(got.rule_id(), "none", "case {case}");
            continue;
        }
        let person = matching.iter().any(|r| r.rule.scope != Scope::Default);
        let strongest = matching
            .iter()
            .filter(|r| !person || r.rule.scope != Scope::Default || r.rule.effect == Effect::Deny)
            .map(|r| r.rule.effect)
            .max()
            .expect("something participates");
        assert_eq!(got.effect(), strongest, "case {case}: `{command}`");
    }
}

// ------------------------------------------------------------ T-PERM-005

fn files(dir: &std::path::Path) -> PolicyFiles {
    PolicyFiles {
        project: Some(dir.join(".cairn").join("permissions.json")),
        user: Some(dir.join("user-permissions.json")),
    }
}

/// T-PERM-005: an "always" answer lands in `.cairn/permissions.json` at mode
/// 0600 and is still in force after a restart (a fresh policy).
#[test]
fn t_perm_005_always_persists_across_restart_at_0600() {
    let dir = tempfile::tempdir().expect("tmp");
    let policy = RulePolicy::new(Mode::Build, files(dir.path()), None, false).expect("policy");
    let req = bash("npm test", Mode::Build);
    assert_eq!(policy.decide(&req).effect(), Effect::Ask);

    let stored = policy
        .remember(&req, Effect::Allow, Scope::Project)
        .expect("remembers");
    assert_eq!(stored.id, "r1");
    assert!(stored
        .note
        .as_deref()
        .unwrap_or("")
        .contains("allow bash: npm test"));
    assert_eq!(
        policy.decide(&req).effect(),
        Effect::Allow,
        "in force at once"
    );

    let path = dir.path().join(".cairn").join("permissions.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).expect("file").permissions().mode() & 0o777,
            0o600
        );
    }
    let text = std::fs::read_to_string(&path).expect("file");
    assert!(text.contains("\"schema_version\": 1"), "{text}");

    // "Restart": a brand-new policy reads the same answer from disk.
    let again = RulePolicy::new(Mode::Build, files(dir.path()), None, false).expect("policy");
    assert_eq!(again.decide(&req).effect(), Effect::Allow);
    // …and only for that command.
    assert_eq!(
        again.decide(&bash("npm install", Mode::Build)).effect(),
        Effect::Ask
    );

    // Ids keep counting.
    let second = again
        .remember(&bash("make", Mode::Build), Effect::Deny, Scope::Project)
        .expect("second");
    assert_eq!(second.id, "r2");
}

/// A session answer is remembered in memory only.
#[test]
fn a_session_answer_writes_nothing() {
    let dir = tempfile::tempdir().expect("tmp");
    let policy = RulePolicy::new(Mode::Build, files(dir.path()), None, false).expect("policy");
    let req = bash("make", Mode::Build);
    policy
        .remember(&req, Effect::Allow, Scope::Session)
        .expect("session");
    assert_eq!(policy.decide(&req).effect(), Effect::Allow);
    assert!(
        !dir.path().join(".cairn").exists(),
        "no file for a session answer"
    );
    policy.reload().expect("reload keeps the session's answers");
    assert_eq!(policy.decide(&req).effect(), Effect::Allow);
}

/// An "always deny" outranks a later "always allow" for the same command
/// (REQ-SAFE-004: nothing a later answer — or a model — says removes a deny).
#[test]
fn a_deny_survives_a_later_allow() {
    let dir = tempfile::tempdir().expect("tmp");
    let policy = RulePolicy::new(Mode::Auto, files(dir.path()), None, false).expect("policy");
    let req = bash("make deploy", Mode::Auto);
    policy
        .remember(&req, Effect::Deny, Scope::Project)
        .expect("deny");
    policy
        .remember(&req, Effect::Allow, Scope::Project)
        .expect("allow");
    assert_eq!(policy.decide(&req).effect(), Effect::Deny);
}

/// A rule file that exists but cannot be understood is an error — silently
/// using no deny rules is the one thing the loader must not do — while one
/// bad rule inside a good file only skips that rule.
#[test]
fn a_broken_file_is_an_error_but_a_broken_rule_is_a_warning() {
    let dir = tempfile::tempdir().expect("tmp");
    let f = files(dir.path());
    let project = f.project.clone().expect("path");
    std::fs::create_dir_all(project.parent().expect("dir")).expect("mkdir");

    std::fs::write(&project, "{ not json").expect("write");
    assert!(RulePolicy::new(Mode::Build, f.clone(), None, false).is_err());

    std::fs::write(&project, r#"{"schema_version":2,"rules":[]}"#).expect("write");
    let err = RulePolicy::new(Mode::Build, f.clone(), None, false).expect_err("version error");
    assert!(err.to_string().contains("schema_version"), "{err}");

    std::fs::write(
        &project,
        r#"{"schema_version":1,"rules":[
            {"id":"r1","effect":"allow","action":"bash","target":{"kind":"command_regex","value":"("}},
            {"id":"r2","effect":"deny","action":"bash:rm *"}]}"#,
    )
    .expect("write");
    let policy = RulePolicy::new(Mode::Build, f, None, false).expect("loads");
    assert_eq!(policy.skipped().len(), 1);
    assert_eq!(policy.skipped()[0].code, "W-PERM-BADREGEX");
    assert_eq!(
        policy.decide(&bash("rm x", Mode::Build)).effect(),
        Effect::Deny
    );
}

/// A project rule beats a user rule for the same request.
#[test]
fn project_scope_outranks_user_scope() {
    let dir = tempfile::tempdir().expect("tmp");
    let f = files(dir.path());
    std::fs::write(
        f.user.as_ref().expect("user"),
        r#"{"schema_version":1,"rules":[{"id":"r1","effect":"allow","action":"bash:make *"}]}"#,
    )
    .expect("user");
    let project = f.project.clone().expect("project");
    std::fs::create_dir_all(project.parent().expect("dir")).expect("mkdir");
    std::fs::write(
        &project,
        r#"{"schema_version":1,"rules":[{"id":"r1","effect":"ask","action":"bash:make *"}]}"#,
    )
    .expect("project");
    // Equal effect order would pick Ask over Allow anyway; make them equal.
    std::fs::write(
        &project,
        r#"{"schema_version":1,"rules":[{"id":"r9","effect":"allow","action":"bash:make *"}]}"#,
    )
    .expect("project");
    let policy = RulePolicy::new(Mode::Build, f, None, false).expect("policy");
    assert_eq!(
        policy.decide(&bash("make all", Mode::Build)).rule_id(),
        "r9"
    );
}
