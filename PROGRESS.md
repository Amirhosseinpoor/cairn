# Cairn — implementation progress

Maps to `SPEC.md` §15.4 milestones and §14.2 traceability. Updated at every checkpoint.

## Status: **M0 (Skeleton & contracts) — complete**

All §15.4 M0 exit criteria pass. Verification (re-run after any change):

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all -- --check` | clean |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings (pedantic, `clippy.toml` tuned) |
| Rustdoc | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace` | 0 warnings |
| Tests | `cargo test --workspace` | **194 passed, 0 failed, 0 warnings** |
| MSRV | `cargo +1.83.0 test --workspace` | 194 passed (D-01 / `rust-version`) |
| Coverage | `cargo llvm-cov --workspace --summary-only` | line **86.63%** total — `cairn-config` **87.06%**, `cairn-core` **94.03%** (both ≥ 70% ✅); regions 87.87%, functions 85.64% |
| Licences/bans | `cargo deny check` | ok (`deny.toml`, D-13) |
| Advisories | `cargo audit` | 0 vulnerabilities in 132 crates |
| Documents | `scripts/lint-docs.sh` | 5 checks green (req→test, codes, links, fences, spec mirror) |
| Schemas | `scripts/check-schemas.sh` | 33 event schemas + config schema, no drift |

### Exit criteria (§15.4 M0)

| Criterion | State |
|-----------|-------|
| T-CFG-001..006, T-CFG-010..030 | ✅ `cairn-config` (54 tests) |
| T-CLI-001..003, T-CLI-020..023 | ✅ `cairn-cli` (40 unit + 27 integration) |
| T-ARCH-001..008 | ✅ `cairn-cli/tests/arch.rs` (12 tests) |
| T-SCHEMA-001..003 | ✅ committed schemas are byte-drift-checked |
| T-TECH-001 | ✅ `deny.toml` + `cargo deny check` |
| coverage ≥ 70% (`cairn-config`, `cairn-core`) | ✅ measured with `cargo-llvm-cov` |
| Workspace + all crates stubbed | ✅ 19 members |
| Config loader + validation + JSON Schema | ✅ layered, §11.5 merge table, `schemas/config.schema.json` |
| CLI skeleton, all subcommands/flags | ✅ 13 subcommands |
| Event bus + event schemas | ✅ 32 kinds → 33 committed schema files |
| Error-code registry | ✅ 153 documented codes ↔ `cairn-core` 1:1 |
| CI (lint, unit, coverage) on 3 OS | ✅ `.github/workflows/ci.yml` |
| ADRs for D-01..D-18 | ✅ `docs/adr/ADR-0001..0018` (+ 0019, 0020) |

### Test inventory

| Crate / suite | Tests |
|---------------|-------|
| `cairn-core` | 48 |
| `cairn-config` | 54 |
| `cairn-tui` | 7 |
| `cairn-eventbus` | 6 |
| `cairn-cli` unit | 40 |
| `cairn-cli` integration (`cli.rs`) | 27 |
| `cairn-cli` architecture (`arch.rs`) | 12 |
| **Total** | **194** |

### Documentation deliverables (§15.6)

| Doc | State |
|-----|-------|
| `docs/adr/ADR-0001..0018` (+ `ADR-0019`, `ADR-0020`) | ✅ one per §2 decision, plus checkpoints (§9.8) and resume (§4.7) |
| `docs/adr/README.md` | ✅ index |
| `docs/spec.md` | ✅ byte-identical mirror of `SPEC.md`, drift-checked |
| `docs/config-reference.md` | ✅ M0 stub (§11.5 layering, env mapping, failure codes) → M5 |
| `README.md` | ✅ quick start, 3 examples, badges, doc links |
| `CHANGELOG.md` | ✅ Keep a Changelog, `[Unreleased]` only |
| `LICENSE` | ✅ Apache-2.0 (D-14) |
| `docs/user-guide.md` (M5), `docs/contributor-guide.md` (M3), `docs/security.md` (M4) | ⏳ later milestones |

## Implemented (code + tests in-tree)

| Area | State | Test IDs covered |
|------|-------|------------------|
| Workspace, lint/format/profile policy (§15.3) | done | T-ARCH-001, T-ARCH-002, T-ARCH-004 |
| `cairn-core`: error codes, exit statuses, ids, events, mode, redactor, cancel tree | done | T-ARCH-002, T-ARCH-004, T-SEC-001/002, registry pattern tests |
| `cairn-core::shutdown` — 500 ms flush budget, no-hang contract | done | T-ARCH-008 |
| `cairn-core::event` — 32 event kinds + JSON Schema generation (REQ-ARCH-009) | done | T-SCHEMA-001, T-SCHEMA-002 |
| `cairn-eventbus`: typed events + critical/droppable lanes (REQ-ARCH-005/006) | done | T-ARCH-005, T-ARCH-006 |
| `cairn-config`: layered load, merge, validation, `--effective`, JSON Schema + flag annotations | done | T-CFG-001..007, T-CFG-010..013, T-CFG-020, T-CFG-030 |
| `cairn-cli`: full command tree + flags, `version`, `config`, `doctor`, `auth`, `mcp`, `sessions` | done | T-CLI-001 (partial), T-CLI-002, T-CLI-020..023, T-CFG-005 |
| `cairn-tui`: slash-command registry (§10.3, REQ-TUI-004) | done | T-CLI-003 |
| Stub crates for all 19 workspace members (compile + document boundaries) | done | T-ARCH-001, T-ARCH-003 |
| CI: `lint`, `unit-int` (ubuntu/macOS/Windows), `coverage`, `msrv` | done | §14.7 matrix, M0 subset |
| ADRs, README, CHANGELOG, LICENSE, config-reference stub, spec mirror | done | §15.6 |

### Not yet implemented (by milestone)

- **M1** — providers, SSE, streaming loop, headless `run`, session store: T-PROV-*, T-SESS-*, T-FAULT-*, T-CLI-010..017
- **M2** — tools, edit/fuzzy, bash, git, checkpoints, permissions: T-TOOL-*, T-EDIT-*, T-CMD-*, T-PERM-*, T-CHK-*
- **M3** — context engine, index, TUI: T-CTX-*, T-TUI-*, T-PROMPT-*
- **M4** — modes, guardrails, sandbox, injection defenses, subagents, MCP, hooks: T-MODE-*, T-SBOX-*, T-SEC-020..030, T-LOOP-*
- **M5** — perf, eval suite, fuzz, docs, release: T-PERF-*, T-EVAL-*, T-OPS-*

### Interim behavior (removed as milestones land)

`cairn run`, `cairn chat`, `cairn resume <id>`, `cairn export`, `cairn update` parse and validate their
flags, then exit `1` with `E-IMPL-STAGE` naming the milestone that delivers them. This code is removed
in the milestone that implements the command; it exists so the M0 CLI surface is complete and testable.

## Known limitations of the M0 delivery

- **CI is written, not yet exercised.** `.github/workflows/ci.yml` mirrors §14.7's `lint`, `unit-int`
  (3 OS) and `coverage` rows; only the Linux leg has been run locally. The macOS/Windows legs and the
  third-party actions (`taiki-e/install-action`, `Swatinem/rust-cache`, `rustsec/audit-check`) are
  unverified until a runner picks them up.
- **Branch coverage is enforced only as LLVM regions.** §14.7 sets both line and branch targets;
  `cargo llvm-cov --branch` is unstable on this toolchain, so the numbers above come from the
  default summary (line / region / function). The CI gate runs `--fail-under-lines` on lines only.
- **T-ARCH-008 exercises a synthetic flush.** `shutdown_with_flush` is tested against an in-memory
  closure; wiring it to the real session writer (and asserting fsync failure there) lands with M1's
  `cairn-session`.
- **`schemas/tools/` is empty** until the M4 tool registry emits its schemas; `check-schemas.sh` only
  notes their absence today.
- **`cargo +1.83.0` prints one line** — `warning: ignoring 'resolver' config table without
  '-Zmsrv-policy'` — because cargo 1.83 predates the MSRV-aware resolver that `.cargo/config.toml`
  configures. It is informational; the lockfile it reads was produced by a newer cargo.

## Spec clarifications applied during M0

1. §11.4.1 gains `[modes] allow_unsafe` (referenced by flag `--dangerously-skip-permissions` / G-M1 but missing from the config schema).
2. §11.1 `--approve-plan` config key normalized to `plans.auto_approve`.
3. New codes introduced (documented in spec): `E-CLI-USAGE` (exit 2 usage errors, REQ-CLI-002),
   `E-SESS-NOTFOUND` (exit 9), `E-IMPL-STAGE` (interim, removed at M5).
4. `E-WEB-TOOMANY_REDIRECTS` renamed `E-WEB-REDIRECTS` (underscore broke the `E-<AREA>-<NAME>` shape).
5. §11.2 gained the CLI-level code paragraph (`E-CLI-USAGE`, `E-SESS-NOTFOUND`, `E-IMPL-STAGE`) and a
   §14.3.2b coverage row — REQ-CLI-002 demands a stable code for every non-zero exit.
6. D-13's licence allow-list gained `Unicode-3.0` (required by `unicode-ident` via `syn`/`proc-macro2`).
7. `cairn-testkit` gained its §3.2 row so T-ARCH-001 can check its imports.
8. T-CFG-003's `run` exit-0 assertion restated as *not exit 2* until M1 (the command is an
   `E-IMPL-STAGE` stub until then).
9. `T-SEC-019`, `T-OPS-004`, `T-PERF-010` gained definition rows (§14.3.6/§14.3.11/§14.3.12).
10. §15.1's `clippy.toml` comment named `too-many-threads = 0`, a key and a lint that do not exist —
    replaced by the keys actually tuned (`msrv`, `too-many-lines-threshold`, `max-struct-bools`).
11. §15.1 pinned `rust-toolchain.toml` to `1.83.0`. Cargo 1.83 cannot parse the `edition2024` manifests
    in the dependency graph (and its resolver cannot avoid them), so the file now requests `stable` +
    components while MSRV 1.83 is enforced by `rust-version`, `.cargo/config.toml`, and an `msrv` CI job.
12. §15.3's Schemas task named `cairn schemas --write`, a subcommand §11.1 does not define — the row now
    names `scripts/check-schemas.sh [--write]`, which runs the real generators.
13. T-PROV-010 pointed at `ADR-007` (no such number) and §4.7 linked no ADR — now `ADR-0020`, linked
    from §4.7.
14. §15.6 lists `docs/spec.md` as "this document" while the deliverable is `SPEC.md` — both exist and
    are byte-identical, enforced by `scripts/lint-docs.sh --check-spec-copy`.

## Next: M1 — Provider + headless loop

`cairn-sse` (hand-written SSE parser, D-04) → `cairn-provider` (`Provide` trait, 5 adapters, retry
matrix §4.5, token/cost accounting) → `cairn-parse` → `cairn-session` (JSONL store) → headless
`cairn run -p …` with text/json/stream-json output and the §11.2 exit codes. M0 hands over a green
baseline: every gate above must stay green as these land.
