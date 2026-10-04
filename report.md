Objective
SPEC.md for Cairn is complete (sections 0–16); implement the spec: build the terminal-native AI coding agent cairn in Rust, tracking progress in PROGRESS.md. Current focus: finish M0 exit criteria (spec §15.4), then move to M1.
Requirements
Fixed naming: product Cairn, binary cairn, dir .cairn/, ignore .cairnignore, instructions AGENTS.md (alias CAIRN.md), env prefix CAIRN_, checkpoint ns refs/cairn/checkpoints/*, config ~/.config/cairn/config.toml, sessions ~/.local/share/cairn/sessions/, cache ~/.cache/cairn/.
Spec conventions: REQ-<AREA>-<NNN> (153 IDs), T-<AREA>-<NNN> (448 IDs), A-01..A-10, OQ-01..OQ-05, D-01..D-18, S-1..S-5, P-01..P-27, R1..R10; §16 self-audit asserts counts, so spec edits must keep §14.2/§16 in sync.
Spec fixes Rust (D-01), ratatui+crossterm, hand-written SSE parser, embedded ripgrep crates, JSONL sessions + SQLite index, shadow-ref git checkpoints; unsafe confined to cairn-sandbox; RFC 2119; concrete values only.
Interim stub subcommands exit 1 with E-IMPL-STAGE naming the delivering milestone.
M0 exit criteria (§15.4): T-CFG-001..006 + 010..030, T-CLI-001..003, T-CLI-020..023, T-ARCH-001..008, T-SCHEMA-001..003, T-TECH-001; coverage ≥70% on cairn-config/cairn-core; deliverables also include "event bus + schemas", "CI (lint, unit, coverage) on 3 OS", and "ADRs for D-01..D-18" (docs/adr/ADR-001..018).
§11.5 list-merge table: scalar = higher replaces; table = key-wise recursive merge (absent keys inherit); array-of-tables = replace entirely; array-of-scalars = replace by default, += = append+dedup order preserved.
§6 requires draft 2020-12 $schema (https://json-schema.org/draft/2020-12/schema) for schemas/tools/*; schemas/config.schema.json is draft-07 (schemars default, unspecified by spec).
Every non-zero exit prints a line with a stable E-* code plus hint unless --quiet (REQ-CLI-002).
§11.4.2 contains no rule requiring non-empty providers.<id>.base_url.
Decisions
Deliverable spec at /home/amir/Desktop/cairn/SPEC.md; implementation follows M0→M5. Rust stable 1.99.0, MSRV rust-version = "1.83", no rust-toolchain.toml.
Workspace members = ["crates/*"], resolver 2, unsafe_code = "deny", clippy pedantic warn, release profile lto=fat/panic=abort/strip; dev-deps in cairn-cli: assert_cmd, predicates, tempfile.
cairn-config: layered loader with per-layer fallback (W-CFG-FALLBACK), unknown fields fatal unless --allow-unknown-keys, cross-field validate(); Source = default|system|user|profile|project|explicit|env|flag; layer order system→user→profile→ancestors far→near→--config→env→flags. Env codes: Bool/BoolNeg/Int→E-CFG-BADENV; Enum/Str/List*→E-CFG-BADVALUE. Fatal sections: "", providers, model, security.
config set/unset edit one TOML document with toml_edit (comments preserved, atomic temp+rename), re-parse as Config + validate() before write; values parsed as TOML (v = <raw>), fallback string; arrays-of-tables rejected (use mcp add/remove).
Keybindings (cairn-config/src/keybindings.rs): <config_home>/keybindings.toml, conflict → E-CFG-KEYCONFLICT + that context's bindings dropped (built-ins restored), reserved alt+f4/cmd+q/ctrl+alt-tab → E-CFG-KEYRESERVED, >2-key chord → E-CFG-BADVALUE.
CLI: clap derive tree, 13 subcommands; try_parse → error: E-CLI-USAGE: ... + hint, exit 2; help/version exit 0; -q/--quiet scanned from raw args on parse failure. run uses clap conflicts_with_all plus validate_static; --offline → E-PROV-OFFLINE exit 3 fast before stub; missing prompt file → E-FS-NOTFOUND exit 9; --stdin on a TTY → usage error. Update variant has disable_version_flag = true.
stdout/stderr via say!/yell! (ignore EPIPE, append newline). Completions render into Vec<u8> then write_all.
auth: key lookup CAIRN_<PROV>_API_KEY → provider-standard env → config providers.<id>.api_key (chmod 0600 + W-CRED-PERM on unix) → workspace .cairn/credentials.toml; sources env|keychain|file; last-4 only; ollama/vllm local.
doctor: all 25 §12.3 rows (PASS/WARN/FAIL/SKIP), summary line, --json, exit 0/1; FAIL rows carry codes (row 2 E-FS-PERM, row 3 first fatal config code, row 4 E-PROV-AUTH); text uses ellipsize(detail, 100).
sessions/resume --list scan <data_home>/sessions/*/*.jsonl first-line headers; unknown id → E-SESS-NOTFOUND exit 9; found id → E-IMPL-STAGE M3. export unknown id → exit 9; --no-redact needs CAIRN_ALLOW_UNREDACTED_EXPORT=1. mcp inspect/refresh → M4; update → M5; export/migrate → M1.
Error codes in cairn-core/src/error.rs: ALL_CODES generated from all 154 spec codes (153 documented + renamed), is_valid_code requires exactly 3 dash-separated uppercase parts; codes module grouped by area with // --- AREA --- comments; collision E-CHK-FAIL→CHK_FAIL, W-CHK-FAIL→W_CHK_FAIL; D-15's E-XXX-YYY deliberately excluded.
Applied SPEC.md patches (all 11 + 1 new): §11.1 config tree (get/set/unset/list forms), plans.auto_approve, §11.2 CLI-codes paragraph, CAIRN_VERIFY_COMMANDS row, [modes] allow_unsafe = false, [trace] debug_unsafe = false, E-CFG-UNSAFEREDACT row, E-WEB-TOOMANY_REDIRECTS→E-WEB-REDIRECTS, §14.3.2b CLI-codes row, D-13 += Unicode-3.0, T-CFG-003 caveat, §3.2 cairn-testkit row; plus §16.1 rewritten (mentions both lint checks, records the 3 dangling T-ids) and §16.5 extended with 11 gap rows and an updated audit-result sentence.
scripts/lint-docs.sh: codes_in_spec() now scans spec_body() (§0–§15) not the whole file, so §16 quotes of codes can't false-fail; reqs_in_spec uses awk '/^## 16\. Final Self-Audit/{exit}'.
deny.toml at workspace root: licenses version 2, allow = MIT/Apache-2.0/BSD-2-Clause/BSD-3-Clause/ISC/Unlicense/Zlib/MIT-0/Unicode-3.0; bans wildcards deny + allow-wildcard-paths = true, multiple-versions warn; sources only crates.io. cargo-deny 0.20.2 installed. Workspace internal deps carry version = "0.1.0" alongside path.
cairn-core/src/lib.rs carries #![forbid(unsafe_code)]; cairn-sandbox/Cargo.toml mirrors the workspace lint table manually with unsafe_code = "allow".
crates/cairn-tui/src/slash.rs: 27-command slash registry 1:1 with §10.3, mode bitflags, lookup/suggest/levenshtein/unknown_message (REQ-TUI-004 ≤2 edits).
cairn-cli/tests/arch.rs: byte-preserving blank_non_code lexer; MAY_IMPORT allow-list mirrors §3.2; dev-deps exempt (documented). rust_sources() now excludes directories named examples/ as well as target/ (the schema generators are build-time executables, not library code).
Architecture checks now scan code_only(...) output (not raw text) for CORE_FORBIDDEN in T-ARCH-002 and for the cairn_tui ident in T-ARCH-003, so comments/doc mentions aren't false positives.
cairn_core::cancel::CancellationToken: eager tree propagation, std-only (Condvar + Mutex<Vec<Weak<Node>>>), API new/child/cancel/is_cancelled/wait_timeout.
cairn_core::shutdown module added (T-ARCH-008): FLUSH_BUDGET = 500ms, shutdown_with_flush(&CancellationToken, FnOnce() -> Result<(), String> + Send + 'static) -> ExitStatus — cancels root first, spawns named cairn-flush thread, polls is_finished() at 1 ms until deadline; returns ExitStatus::Ok, or ExitStatus::Flush (code 13) on Err/panic/timeout (thread detached → "no hangs"). Exported from lib.rs (pub use shutdown::{shutdown_with_flush, FLUSH_BUDGET}); 5 tests.
Event schemas (REQ-ARCH-009): cairn_core::event::event_schemas() -> Vec<(String, String)> sorted by filename — event.schema.json (envelope, oneOf over all 32 variants) plus <kind>.schema.json (envelope narrowed to one variant via single["oneOf"] = json!([variant]), title = kind). Generator: cargo run -p cairn-core --example dump_event_schemas -- schemas/events. 33 files written to schemas/events/ (316K; definitions duplicated per file by design).
T-SCHEMA-002 rewritten as an exact byte-level drift check committed vs event_schemas() (stale/missing/differing all assert with the regeneration hint).
T-SCHEMA-001 now asserts draft 2020-12 for schemas/tools/*, plus non-vacuous M0 guards: schemas/config.schema.json and schemas/events/event.schema.json must exist; tool-content check still early-returns until M4.
T-CFG-011 added in crates/cairn-config/src/load.rs (t_cfg_011_merge_semantics_table) using ancestor-project (lower) vs workspace-project (higher) layers, asserting all four §11.5 table rows incl. discovery.exclude += [...] → ["node_modules/**", "docs/**"].
cargo-llvm-cov 0.9.1 + llvm-tools-preview installed; measured per-crate line coverage: cairn-config 86.62%, cairn-core 91.47%, cairn-tui 99.36%, cairn-eventbus 86.11%, cairn-cli 82.64%, TOTAL 86.25% (all ≥70% ✓).
Test fixture isolation: CAIRN_HOME + CAIRN_WORKSPACE temp dirs, current_dir(ws), env_remove every ENV_KEYS entry plus CAIRN_CONFIG, NO_COLOR, provider *_API_KEY vars.
LoadOptions has no user-config override, so merge tests use two project layers (parent dir lower, workspace .cairn/config.toml higher).
Work State
Completed
SPEC.md sections 0–16 (4,300+ lines, 153 REQ, 448 T-IDs) plus all queued patches; scripts/lint-docs.sh now exits 0 on all 3 checks (req coverage: 153 requirements, all mapped to tests / test ids: 184 referenced, all defined / codes: 153 documented, registry is 1:1).
§16.5 "Gaps found and fixed in place" extended with 11 new rows (approve-plan key, [modes] allow_unsafe, trace.debug_unsafe + E-CFG-UNSAFEREDACT, CAIRN_VERIFY_COMMANDS, E-WEB-REDIRECTS rename, CLI-level codes, D-13 Unicode-3.0, cairn-testkit row, T-CFG-003 caveat, 3 dangling T-ids); §16.1 rewritten.
Workspace Cargo.toml, .gitignore, PROGRESS.md; assets/models.json; deny.toml; schemas/config.schema.json.
cairn-core green 54 tests (incl. 8 cancel, 5 shutdown/T-ARCH-008, event_schemas_cover_every_kind, #![forbid(unsafe_code)], CODE_AREAS 43 areas).
cairn-eventbus 6 tests; cairn-config green 54 tests (incl. new T-CFG-011); cairn-tui 7 tests; cairn-cli 40 unit + 27 integration + 12 arch tests.
Workspace total: 199 tests passed, zero warnings, zero failures (last full run: 192 before T-CFG-011/shutdown; cairn-config now 54).
Coverage ≥70% criterion verified for cairn-config (86.62%) and cairn-core (91.47%) via cargo llvm-cov --workspace --summary-only.
Event schemas generated and drift-tested (T-SCHEMA-002 real, T-SCHEMA-001 real for M0 half).
cargo deny check bans → ok; cargo deny check licenses sources → ok (5 benign license-not-encountered warnings).
Active
M0 deliverables still outstanding: (a) ADRs docs/adr/ADR-001..018 for D-01..D-18, (b) CI .github/workflows/ci.yml (lint, unit, coverage on 3 OS), both named §15.4 deliverables.
PROGRESS.md not yet updated (M0 status, test counts, coverage numbers, spec-clarifications log, T-ARCH-008 now delivered).
T-ARCH-008 shutdown module's fsync-failure injection is only exercised via a simulated Err closure; real session-writer integration lands with M1 (should be noted in PROGRESS).
Blocked
(none)
Next Move
Write docs/adr/ADR-001..018 covering D-01..D-18 (M0 deliverable).
Add .github/workflows/ci.yml (lint, unit, coverage across Linux/macOS/Windows; include scripts/lint-docs.sh, cargo deny, cargo llvm-cov thresholds) and smoke-check the YAML.
Rewrite PROGRESS.md: M0 complete, 199+ tests, coverage 86.62%/91.47% (86.25% total), spec-clarifications log, T-ARCH-008 note, pending M0 items.
Re-run full gate: cargo test --workspace, cargo clippy --workspace, ./scripts/lint-docs.sh, cargo deny check.
Move to M1 crates: cairn-sse, cairn-provider, cairn-parse, cairn-session.
Relevant Files
/home/amir/Desktop/cairn/SPEC.md: source of truth; §11.5 merge table ~3700s, §14.2 matrix ~3320–3477, §14.3 test tables ~3600–3935, §15.4 milestones, §16 audit 4253+.
/home/amir/Desktop/cairn/scripts/lint-docs.sh: §16.1 lint (3 checks, now green); codes_in_spec() uses spec_body.
/home/amir/Desktop/cairn/crates/cairn-cli/tests/arch.rs: T-ARCH-001..007, T-SCHEMA-001..003, T-TECH-001; lexer blank_non_code, rust_sources skips examples/.
/home/amir/Desktop/cairn/crates/cairn-core/src/shutdown.rs: T-ARCH-008 shutdown budget (new).
/home/amir/Desktop/cairn/crates/cairn-core/src/event.rs: event_schemas() generator + completeness test; EventData::ALL_KINDS (32 kinds).
/home/amir/Desktop/cairn/crates/cairn-core/examples/dump_event_schemas.rs: schema generator entry point.
/home/amir/Desktop/cairn/schemas/events/: 33 committed event schemas (T-SCHEMA-002 drift target).
/home/amir/Desktop/cairn/crates/cairn-config/src/load.rs: t_cfg_011_merge_semantics_table, merge_tables, extract_appends, dedup_extend.
/home/amir/Desktop/cairn/crates/cairn-core/src/error.rs: 154-code registry, ALL_CODES, ExitStatus (incl. Flush = 13).
/home/amir/Desktop/cairn/crates/cairn-cli/tests/cli.rs: M0 integration suite (27 tests).
/home/amir/Desktop/cairn/PROGRESS.md: milestone tracking; needs full update.
/home/amir/Desktop/cairn/deny.toml: D-13 license/bans/sources policy.
/home/amir/Desktop/cairn/schemas/config.schema.json: committed config schema, asserted equal to cairn_config::json_schema_pretty() by T-SCHEMA-003.
/home/amir/Desktop/cairn/docs/adr/: (does not yet exist) target for ADR-001..018.
/tmp/cov.txt: last cargo llvm-cov --workspace --summary-only output (per-file coverage).
Important Context
Run tests with . "$HOME/.cargo/env" first, e.g. cd /home/amir/Desktop/cairn && . "$HOME/.cargo/env" && cargo test --workspace.
Coverage: cargo llvm-cov --workspace --summary-only (per-file rows are 13 awk fields; lines=$8, missed=$9); cargo-llvm-cov 0.9.1 and llvm-tools-preview are installed.
cargo-deny 0.20.2 at ~/.cargo/bin/cargo-deny; path-only workspace deps read as wildcards unless version present.
println!/eprintln! banned in the CLI crate — use say!/yell!.
Raw strings in Rust cannot contain \"; nested r#" inside r#" terminates early — use r##"..."##.
Python3 available but no toml module.
doctor exits 1 on a keyless machine (E-PROV-AUTH).
Shell-tool stdout/stderr may be reported separately, making output order look interleaved.
Not a git repo; all files uncommitted/untracked.
Last full cargo test --workspace before the newest additions: 192 passed, 0 failed, 0 warnings; cairn-config alone is now 54.
cairn_config::load::Loaded::fatal_issues() returns Vec<&Issue> — use .into_iter().map(Issue::render), not .iter().
Animation enum variants are Auto/On/Off (not bool); McpServer fields: name, transport, command, args, env, url, headers, working_directory.
