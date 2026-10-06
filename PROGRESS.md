# Cairn — implementation progress

Maps to `SPEC.md` §15.4 milestones and §14.2 traceability. Updated at every checkpoint.

## Status: **M0 (Skeleton & contracts) — complete · M1 (Provider + headless loop) — in progress**

M0 remains green; every gate below is re-run after each M1 change. M1 currently has the
session store (`cairn-session`) with the three commands built on it (`sessions`, `export`,
`migrate`), the hand-written SSE parser (`cairn-sse`, D-04), and `cairn-provider`'s boxed
`Provider` trait with the §4.5 taxonomy as data, the §4.5 retry policy (`cairn-provider::retry`)
as pure, testable values, §4.8/§4.9's registry and cost accounting, the per-adapter wire
decoder (`cairn-provider::wire`: §4.2's three stream shapes into §3.4's `StreamEvent`s),
the §4.3 transport (D-04 `reqwest` client, status mapping, SSE/NDJSON framing into the
decoder), and the five §4.4 adapters with non-network `health()` — see
[M1 progress](#m1-progress). CI is green on all three OSes plus lint, coverage and MSRV.

### Gates (re-run after any change)

| Gate | Command | Result |
|------|---------|--------|
| Format | `cargo fmt --all -- --check` | clean |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings (pedantic, `clippy.toml` tuned) |
| Rustdoc | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace` | 0 warnings |
| Tests | `cargo test --workspace` | **423 passed, 0 failed** |
| MSRV | `cargo +1.83.0 test --workspace` | 392 passed (D-01 / `rust-version`) |
| Coverage | `cargo llvm-cov --workspace --summary-only` | line **86.63%** total — `cairn-config` **87.06%**, `cairn-core` **94.03%** (both ≥ 70% ✅); regions 87.87%, functions 85.64% (M0 measurement) |
| Licences/bans | `cargo deny check` | ok (`deny.toml`, D-13) |
| Advisories | `cargo audit` | 0 vulnerabilities in 154 crates |
| Documents | `scripts/lint-docs.sh` | 6 checks green (req→test, test ids, codes, links, fences, spec mirror) |
| Schemas | `scripts/check-schemas.sh` | 33 event schemas + config schema, no drift |

### Exit criteria (§15.4 M0)

| Criterion | State |
|-----------|-------|
| T-CFG-001..006, T-CFG-010..030 | ✅ `cairn-config` (54 tests) |
| T-CLI-001..003, T-CLI-020..023 | ✅ `cairn-cli` (50 unit + 39 integration) |
| T-ARCH-001..008 | ✅ `cairn-cli/tests/arch.rs` (12 tests) |
| T-SCHEMA-001..003 | ✅ committed schemas are byte-drift-checked |
| T-TECH-001 | ✅ `deny.toml` + `cargo deny check` |
| coverage ≥ 70% (`cairn-config`, `cairn-core`) | ✅ measured with `cargo-llvm-cov` |
| Workspace + all crates stubbed | ✅ 19 members |
| Config loader + validation + JSON Schema | ✅ layered, §11.5 merge table, `schemas/config.schema.json` |
| CLI skeleton, all subcommands/flags | ✅ 14 subcommands (13 at M0; `init` added in M1 — §16.5) |
| Event bus + event schemas | ✅ 32 kinds → 33 committed schema files |
| Error-code registry | ✅ 158 documented codes ↔ `cairn-core` 1:1 |
| CI (lint, unit, coverage) on 3 OS | ✅ `.github/workflows/ci.yml` |
| ADRs for D-01..D-18 | ✅ `docs/adr/ADR-0001..0018` (+ 0019, 0020) |

### Test inventory

| Crate / suite | Tests |
|---------------|-------|
| `cairn-core` | 58 |
| `cairn-config` | 57 |
| `cairn-tui` | 7 |
| `cairn-eventbus` | 6 |
| `cairn-session` (M1) | 44 |
| `cairn-sse` (M1) | 23 |
| `cairn-provider` (M1) | 96 |
| `cairn-cli` unit | 50 |
| `cairn-cli` integration (`cli.rs`) | 39 |
| `cairn-cli` architecture (`arch.rs`) | 12 |
| **Total** | **392** |

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
| `cairn-cli`: full command tree + flags, `version`, `config`, `doctor`, `auth`, `mcp` | done | T-CLI-001 (partial), T-CLI-002, T-CLI-020..023, T-CFG-005 |
| `cairn-session`: JSONL store, read contract, migration, export, GC (§11.7) | done (M1) | T-SESS-010, T-SESS-011, T-SESS-012, T-SESS-020, T-SESS-023, T-SESS-030, T-SESS-031 |
| `cairn-sse`: §4.3 framing (partial lines across reads, CRLF, `id`/`retry` recorded, 1 MiB cap, EOF dispatch), 45 s idle timer, 250 ms cancellation | done (M1) | T-PROV-020, T-PROV-021, T-PROV-022, T-PROV-023, T-PROV-024, T-PROV-032 |
| `cairn-provider`: boxed `Provider` trait (§3.4), `Capabilities`/`StreamEvent`/`ModelRequest`/`ProviderId`, and §4.5's retry matrix as values instead of five `match`es | done (M1) | §3.4, §4.2, §4.5, §4.8 |
| `cairn-provider::retry` — §4.5's backoff formula as `delay_bounds`, `Retry-After` (seconds *and* HTTP-date), the 180 s `RetryBudget`, and an RNG that point ranges never reach | done (M1) | §4.5, REQ-PROV-005, T-PROV-005, T-PROV-040 |
| `cairn-core::registry` — §4.9's `models.json` parsed once (two consumers that may import only `core`), embedded at compile time, with alias resolution and `auth_header` split into name + prefix | done (M1) | §4.9, REQ-PROV-013, T-PROV-013 |
| `cairn-provider::accounting` — §4.8's estimator (the branch predicate §4.8 named but did not state), `estimate_request`, and REQ-PROV-012's cost formula with `null` for an unpriced model | done (M1) | §4.8, REQ-PROV-012 |
| `cairn-provider::wire` — `WireDecoder` turns one provider's payloads (SSE `data` for Anthropic/`OpenAI`, one NDJSON line for Ollama) into §3.4's `StreamEvent`s | done (M1) | T-PROV-001, T-PROV-025, T-PROV-026, T-PROV-028, T-PROV-029, T-PROV-035, §4.2, §4.3, §4.4 |
| `cairn-provider::transport` + five §4.4 adapters — D-04 `reqwest` client (bundled Mozilla roots, `ca_bundle` adds a site CA), status→fault with 400 refinement and `Retry-After` capture, SSE framing plus NDJSON reframed through the same idle/cancel policy, mid-stream failures as terminal `Finish { stop: Error }` with the fault on a side channel; Anthropic/OpenAI/compat/Ollama/vLLM adapters with §4.4 shaping, non-network `health()`, and `T-ARCH-006`-clean HTTP (build-then-execute) | done (M1) | T-PROV-003, T-PROV-034, T-PROV-037, T-PROV-045, §4.4, §4.5, §4.10 |
| `cairn-cli`: `sessions` on the real store, `export`, `migrate`, `resume <id>` load check | done (M1) | T-CLI-015, T-SEC-012 |
| `cairn run -p` one-shot — prompt from `-p`/file/stdin, `provide::build` bridging (registry + base_url/api_key/ca_bundle overrides, model limits, capped retry budget), current-thread runtime with Ctrl-C watcher, text/json/stream-json output, exit 0/3/7 with the fault's stable code | done (M1) | T-CLI-010, T-CLI-017, T-CLI-020 |
| `cairn-tui`: slash-command registry (§10.3, REQ-TUI-004) | done | T-CLI-003 |
| Stub crates for all 19 workspace members (compile + document boundaries) | done | T-ARCH-001, T-ARCH-003 |
| CI: `lint`, `unit-int` (ubuntu/macOS/Windows), `coverage`, `msrv` | done | §14.7 matrix, M0 subset |
| ADRs, README, CHANGELOG, LICENSE, config-reference stub, spec mirror | done | §15.6 |

### Not yet implemented (by milestone)

- **M1** — `cairn-parse`, the five `cairn-provider` adapters (§4.4 shaping over the landed trait),
  the §4.5 retry loop, a mock provider with cassettes, the headless `run -p` loop with
  text/json/stream-json and the §11.2 exit codes, logging + redaction (§12.1): T-PROV-*, T-FAULT-*,
  T-CLI-010..017, T-SEC-002, T-ARCH-005..008 (the narrowed §15.4 span). The session half (T-SESS-*),
  `cairn-sse` (D-04), §4.8/§4.9's registry + accounting and the boxed `Provider` trait have landed —
  see below.
- **M2** — tools, edit/fuzzy, bash, git, checkpoints, permissions: T-TOOL-*, T-EDIT-*, T-CMD-*, T-PERM-*, T-CHK-*
- **M3** — context engine, index, TUI: T-CTX-*, T-TUI-*, T-PROMPT-*
- **M4** — modes, guardrails, sandbox, injection defenses, subagents, MCP, hooks: T-MODE-*, T-SBOX-*, T-SEC-020..030, T-LOOP-*
- **M5** — perf, eval suite, fuzz, docs, release: T-PERF-*, T-EVAL-*, T-OPS-*

### Interim behavior (removed as milestones land)

`cairn run`, `cairn chat`, `cairn init`, `cairn resume <id>` and `cairn update` parse and validate
their flags, then exit `1` with `E-IMPL-STAGE` naming the milestone that delivers them. This code is
removed in the milestone that implements the command; it exists so the CLI surface is complete and
testable — including the command `cairn doctor` points people at.
(`cairn export` and `cairn migrate` were the last of these and are now real.)

## M1 progress

### Landed

| Area | State | Test IDs |
|------|-------|----------|
| `cairn-session` — `Record`/`Header` (§11.7), JSONL append with `write` + `fsync` (REQ-LOOP-006), `0600` on create, atomic `save` (REQ-CLI-009) | done | T-SESS-010, T-SESS-020 |
| Read contract — torn last line discarded, mid-file corruption → `E-SESS-CORRUPT` with the file left byte-identical, non-UTF-8 → `E-FS-ENCODING` | done | T-SESS-023 |
| Migration — `v0 → v1`, backup first, no downgrade, unknown records preserved verbatim | done | T-SESS-011, T-SESS-012 |
| Export — `md` / `json` / `html`, redaction before markup, images exported as `[image: <type>]` | done | T-SESS-031, T-SEC-012 |
| GC — 90 days / 500 sessions per workspace, tombstone then unlink after 7 days, `in_progress` plans protected, 24 h throttle | done | T-SESS-030 |
| `cairn sessions` — rebuilt on `Store::list_all` (all workspace dirs, newest first, `--json/--limit/--workspace/--grep`) | done | §11.1 |
| `cairn export` — real, with `--format`, `--output PATH`, `--no-redact` confirmation (exit 2 outside a TTY without `CAIRN_ALLOW_UNREDACTED_EXPORT=1`) | done | T-SESS-031, T-SEC-012, T-CLI-015 |
| `cairn migrate` — sessions **and** every `config.toml` layer, run before the startup checks so it can repair the config `validate` rejects | done | §11.7.1, REQ-CLI-009 |
| `cairn resume <id>` — loads the session first, so a corrupt file reports `E-SESS-CORRUPT` (exit 9) instead of "not implemented" | done | T-SESS-023 |
| `cairn init [--global]` — the subcommand §11.1 was missing while P3, §4.10, §7.3, REQ-SAFE-003, T-SEC-014 and `cairn doctor`'s "no AGENTS.md" hint all told people to run it; it parses and reports `E-IMPL-STAGE` M3 (delivered with §10.3's `/init`) | done | §11.1, T-SEC-014 |
| `[migrate] auto = true` — the §11.7 config key that §11.4.1 (which claims to list every key) did not define; `cairn config` would have rejected it | done | §11.4.1, §11.7 |
| `hints_only_name_real_subcommands` — every ``cairn <cmd>`` that appears in a hint must be a real subcommand, so the `cairn init` gap cannot recur | done | §12.3 |
| `cairn-sse` — a pure `SseParser` (bytes in, events out, no timers) plus an async `SseStream` that owns the idle deadline and the cancel poll; §4.3 rules 1–6 implemented, `E-PROV-EVENTBIG`/`E-PROV-IDLE` reported as codes rather than strings | done | T-PROV-020, T-PROV-021, T-PROV-022, T-PROV-023, T-PROV-024, T-PROV-032 |
| `cairn-provider` — the boxed `Provider` trait exactly as §3.4 specifies it, `ProviderId`/`Capabilities`/`StreamEvent`/`ModelRequest`/`ToolSpec`/`ProviderHealth`, and `TokenCount` as an alias of `Usage` | done | §3.4, §4.2, §4.9 |
| `ProviderFault` + `ProviderError` — §4.5's matrix as values (code, retryable, attempt budget, backoff shape), with a test that the 19 rows cover exactly the 18 `E-PROV-*` codes the registry holds | done | §4.5, §16.4 |
| `Usage::estimate` / `Usage::reported` — REQ-PROV-011's flag can only be set at construction, and `Default` would have produced an *unflagged* estimate | done | §4.8 |
| `cairn-provider::retry` — `delay_bounds(attempt, fault, retry_after)` returns `None` exactly where §4.5's *Retries* column says not to retry, `Backoff` picks the row's shape (full jitter / point / fixed / rate-limited floor), and `RetryBudget::afford` refuses rather than sleep past REQ-PROV-005's deadline | done | §4.5, T-PROV-005, T-PROV-040, T-PROV-046 |
| `cairn-core::registry` — §4.9's document in `cairn-core` because its two consumers (`cairn-config` for REQ-PROV-013, `cairn-provider` for §4.2/§4.8) may import only `core`; `include_str!` keeps it compile-time, so the module still performs no I/O. `resolve()` matches an id first then an alias, `Resolved` carries the provider entry an adapter needs for its URL, and a test asserts every alias in the shipped file resolves to exactly one model | done | §4.9, REQ-PROV-013 |
| `registry.resolve_id` in `cairn-config::validate` — `model = "sonnet"` reached `E-CFG-NOMODEL` because only canonical ids were compared, and the `max_output` lookup keyed on the raw string would have missed too; both now go through the alias, so an alias validates *and* carries its canonical limits | done | REQ-PROV-013, T-CFG-007 |
| `models_path` read at load — the key was specified in §4.9 and §11.4.1 and read by nothing. An unusable override now warns `W-REG-FALLBACK` (a code that was registered and tested but emitted by no path) and falls back to the bundle, §11.4.2's absolute-or-`~` rule reaches it, and `cairn config set models_path …` refuses a value the next startup would discard | done | §4.9, REQ-PROV-014, T-PROV-014 |
| `cairn-provider::accounting` — `estimate_tokens` applies §4.8's two formulas with the predicate the section named but never stated, `estimate_request` sums messages and tool schemas, and `cost_usd` returns REQ-PROV-012's formula as `Option<f64>` (`null` when §4.9's price is `null`, never `0.0`); `Capabilities::from_entry` is T-PROV-003's "equals the registry row" | done | §4.8, REQ-PROV-011, REQ-PROV-012 |
| `cairn-provider::wire` — per-adapter wire decoder (§4.2's three stream shapes → §3.4's `StreamEvent`s): `Finish` only from `finish()` so `Usage` precedes it, one merged `Usage` only when the provider's numbers are complete, §4.3's ≥ 5 malformed abort with keep-alive and `[DONE]` uncounted, synthetic `ToolCallStart` (`synthetic-{index}`, empty name) for unknown indices, EndTurn→ToolUse when tools were seen (§4.4 row 6, both directions), no `ToolCallEnd`/`Finish` for a cut stream; `ProviderFault::from_status` maps numeric in-band codes; `ReasoningSignature` feeds §4.1's `Block::Reasoning.signature`; Responses API (`response.output_text.delta`) recorded outstanding | done | T-PROV-001, T-PROV-025, T-PROV-026, T-PROV-028, T-PROV-029, T-PROV-035, §4.2, §4.3, §4.4 |
| `cairn-provider::transport` — D-04's client and the POST path: `reqwest` 0.12 with `default-features = false` (`rustls-tls-webpki-roots`, `stream`, `gzip`, `http2`), 10 s connect timeout, `ca_bundle` PEM added to the bundle; non-2xx mapped by `error_for_status` (exact `E-PROV-AUTH` message, 400 refinement to `ContextLength`/`ContentFilter`, `Retry-After` captured on 429); bytes pumped through `SseStream` (idle + 50 ms cancel poll) into the decoder, Ollama's NDJSON reframed line-by-line so the same policy applies; mid-stream failures end with `Finish { stop: Error }`, EOF-without-terminator records `Unreachable` (§4.7), cancellation ends silently; covered live against loopback | done | §4.3, §4.5, T-PROV-034, T-PROV-037, T-PROV-045 |
| Five §4.4 adapters — Anthropic/OpenAI/compat/Ollama/vLLM over one `Core`: registry resolution (alias-aware, `""` base_url means default), `health()` with no network (`UnknownModel`/`Misconfigured`/`NoCredentials`, Ollama keyless per §4.10), `stream()` refusing before any socket, `capabilities()` from the registry row, `count_tokens()` local; pure shaping snapshots (Anthropic system breakpoint + tool blocks, OpenAI `include_usage` + o-series `max_completion_tokens`, Ollama object args + `num_predict`); `env_key` covers §4.10 steps 1–2 | done | T-PROV-003, §4.4, §4.10 |
| §4.5 retry loop — `stream_with_retry` drives `Provider::stream` under the matrix: attempts never overlap, setup and mid-stream faults share one path, `ContextLength` compacts once and resends immediately (fatal on the second), backoffs sleep in 250 ms steps so cancel wins (REQ-PROV-006), budget exhaustion and fatal faults end the turn with one synthesised `Finish { stop: Error }` when no bytes flowed, cancellation always silent; new `Truncated` fault (shares `E-PROV-NET`, one retry) for clean EOF without terminator or stop | done | T-FAULT-001, T-FAULT-004, T-FAULT-006, T-PROV-006, T-PROV-033, T-PROV-037, §4.5, REQ-PROV-005, REQ-PROV-009 |
| Mock provider + cassettes + §4.6 fallback — `MockProvider` plays committed `assets/cassettes/*.json` scripts through the real `WireDecoder` (payloads, setup errors, mid-stream faults with recording, per-call buffering); `fallback_section`/`extract_prompt_tool_call`/`format_tool_result` implement §4.6 exactly (first block wins, bad JSON classifies for the turn loop's two-repair budget), and adapters with `tool_calling == false` shape tools into the system prompt instead of native parameters; live loopback proofs for T-PROV-006 (cancel ≤ 250 ms), T-PROV-009 (disconnect replays whole turn, 2 connections), T-PROV-033 (500/503/200 across 3 connections), T-PROV-011 (late usage overrides estimate in cost) | done | T-PROV-001, T-PROV-004, T-PROV-006, T-PROV-009, T-PROV-011, T-PROV-033, T-PROV-035, T-FAULT-001, §4.6, §4.7 |
| `cairn-provider::assemble` — `ToolCallAssembler` folds `ToolCallStart`/`Delta`/`End` into `Block::ToolCall`s (synthetic id for an unknown index, parallel indices, open calls never emitted on a cut stream); `parse_tool_args` applies §4.3's repairs in one string-aware pass — close braces/brackets to depth 8, strip trailing commas, `NaN`/`Infinity` → `null` — and reports the *original* parse error as `parse_error` | done (M1) | T-PROV-002, T-PROV-030, T-PROV-031 |
| `cairn-provider::fallback::FallbackBudget` — REQ-PROV-008's counter as plain state the turn loop owns: two bad `<tool>` blocks per turn are repaired, the third disables the fallback for the session (`E-PROV-FALLBACK`); a good block refunds nothing, a new turn resets the count but never the disable | done (M1) | T-PROV-008 |
| User-defined models — REQ-PROV-013's `models.<id>` escape hatch now *runs*, not just validates: `provide::build` synthesises the registry row from the id's provider prefix and the user's `context_window` (§4.9 amended: the spec said the model validates but never said which provider or capabilities it gets) | done (M1) | REQ-PROV-013, T-PROV-013 |
| `run --output json` usage honesty — the result now carries `usage.estimated` and `cost_usd` (§7.7): provider numbers override (REQ-PROV-011); a missing or all-zero report falls back to the §4.8 estimator flagged `estimated`; an unpriced model gives `cost_usd: null`, never `0` | done (M1) | T-FAULT-005, T-PROV-012, REQ-PROV-011/012 |
| §4.5 fatal HTTP rows, live — 401/403/404/405 and a 400 content-filter each make exactly one connection, carry their stable code, and still end the turn with one `Finish { stop: Error }` | done (M1) | T-PROV-034, T-PROV-038, T-PROV-039, T-PROV-043, T-PROV-044 |

### Decisions taken while landing the session store

1. **`E-SESS-FLUSH` added** (157 → 158 codes): REQ-CLI-002 demands a stable `E-*` code for *every*
   non-zero exit, and a record that cannot be written/fsynced (exit 13) had none.
2. **`message` records nest the `Message`** under `message` with `turn_id` beside it — §11.7 lists this
   record's fields as `seq`, `turn_id`, `message`, so flattening would not match the table.
3. **`header.ruleset_version` and `header.parent_session` are mandatory, `plan_id` is not** — the §11.7
   table marks only `plan_id` with `?`. They are therefore written on every header (`null` for a first
   session); dropping them in a rewrite would make the next read corrupt.
4. **`cairn migrate` is dispatched before `emit_startup_issues`.** A wrong `schema_version` is exactly
   what `validate` reports as fatal, so a repair command waiting behind that report could never run on
   the only files it exists to fix.
5. **`--output` carries two meanings** (§11.1 global table: format; §11.1 export signature: destination).
   clap allows one long per command, so export reads the *global* value back as a path and skips it when
   building the `output.format` override (which is an enum and would reject every path).

### Spec amendments landed with M1 (all recorded in §16.5)

Items 1–3 were written before the provider work started, because two of them decide *where that
code goes*. Items 4 and 9–18 were found while writing it — the pattern is the same as §16.5's
own: a section that claims completeness, and does not have it.

1. **§11.8 was at the end of the file**, after §16.5's audit result, though §0, §14.3.10 and
   `docs/config-reference.md` all cite it as §11.8 → moved between §11.7 and §12.
2. **§3.4 put `Provider` in `cairn-core/src/provider.rs` and `Tool` in `cairn-core/src/tool.rs`**,
   while §3.2 assigns the `Provider` trait to `cairn-provider` and the `Tool` trait to `cairn-tools`,
   and gives `cairn-core` the dependency row `std, serde, thiserror` — which cannot express the
   `BoxStream` the same block already returns. §3.4's path comments now name the §3.2 crates, so
   `cairn-core` keeps its dependency row and the M1 trait lands in `cairn-provider`.
3. **§3.4's `async fn` signatures are not object-safe** (the return type names `Self`), so the
   registry's `Box<dyn Provider>` and `Arc<dyn Tool>` would not compile → `Provider::stream`,
   `Provider::count_tokens` and `Tool::execute` now return `futures::future::BoxFuture<'a, …>`,
   with §3.4 stating which traits need boxing and why.
4. **D-02's `CancellationToken`: the amendment made here was wrong and has been reversed.** The
   first draft added a `tokio-util` 0.7 row to §15.2 on the reasoning that `tokio` does not ship the
   token. It does not — but `cairn-core::cancel::CancellationToken` already *is* the one D-02 tree,
   std-only by §3.2 and covered by T-ARCH-007's tests, so a second token type would have violated
   D-02's "one tree". §15.2 lists no crate for it; REQ-ARCH-007 now names `token.is_cancelled()`, the
   predicate that type actually has; and §3.4 says outright which token every signature means.
   `cairn-sse` polls it every 50 ms — which is why §4.3's 250 ms budget and REQ-ARCH-007's 100 ms
   polling floor both hold.
5. **`cairn init` did not exist in §11.1** despite six references, one of them normative
   (REQ-SAFE-003) → added as `cairn init [--global]`, the CLI form of §10.3's `/init`, stubbed to M3.
6. **`migrate.auto` was referenced by §11.7 but absent from §11.4.1** → `[migrate] auto = true` added
   to the config listing and to `cairn-config` (`MigrateConfig`, schema regenerated).
7. **`cairn-parse`'s manifest described §4.3's tool-argument parsing**, which §3.2 assigns to
   `cairn-provider` → manifest and crate docs now say "Tree-sitter wrapper: grammars, queries,
   syntax validation (SPEC 5.2, 6.3.6, 6.7.4)".
8. **§11.1 lists `--output` twice with two meanings** → one flag with two readings (decision 5 above);
   a format spelling at `export` is refused with `E-CLI-USAGE` rather than creating a file called `json`.
9. **§3.2 gave `cairn-sse` the MAY-import list "tokio, bytes, thiserror"** — no workspace crate at all —
   while §4.3 routes `E-PROV-EVENTBIG` and `E-PROV-IDLE` through it and its manifest depends on
   `cairn-core`, which `T-ARCH-001` already allowed → `core` added to the MAY row.
10. **`cairn-sse`'s manifest cited "SPEC 4.4"**, which is request shaping; its contract is §4.3 →
    corrected.
11. **§4.3 said nothing about end of stream**, so a server that omits the final blank line would lose
    `[DONE]`/`message_stop`, and nothing about how to decode bytes that straddle reads → rules 5 and
    6 added: decode per event (so a split UTF-8 sequence is not mistaken for invalid bytes) and
    dispatch a pending event or final line at EOF, a deliberate departure from WHATWG's discard.
12. **§4.5's matrix covered 15 of the 18 `E-PROV-*` codes** — `E-PROV-EVENTBIG`, `E-PROV-FALLBACK`
    and `E-PROV-OFFLINE` had no row — and had no row at all for a connection lost mid-stream, which
    §4.7 defines and `cairn-sse` actually produces → four rows added; §4.5 now closes over §16.4's
    claim that it holds "provider paths".
13. **§3.4 wrote `fn stream<'a>(&'a self, …)` and `fn execute<'a>(&'a self, …)`** where the named
    lifetime is exactly what `clippy::needless_lifetimes` rejects, and `clippy -D warnings` is a gate →
    both elided (same bound), `count_tokens` left explicit because it must tie `&self` to a second
    borrowed input, with §3.4 saying which is which so neither form gets "restored".
14. **§4.3's "log `warn`" for lossy UTF-8 could not be done from `cairn-sse`**, which carries no
    logger (and no §15.2 row gives it one) → the row now says the parser counts substitutions and
    `cairn-provider` emits the `warn`, which is where §12.1's logging lives anyway.
15. **§4.5's matrix column was headed *Attempts* while §4.5's own formula ran `n = 1..5` and
    §11.4.1's knob was `max_retries = 5`**, and `E-PROV-MALFORMED`'s note called that same number
    "1 retry" — so one number was a retry count in two places and an attempt count in a third.
    The column is now **Retries** (backoffs taken; a call makes one HTTP request more than that),
    D-05 and T-PROV-005/040/046 are reworded to match, and §10.1/§10.9's status line reads
    `retry 1/5` instead of `attempt 2/5`, which was off by one against the column it illustrated.
    `model.error`'s `attempt` field keeps its name — it counts HTTP requests.
16. **T-PROV-005's expected cell read "total wait ≥ 7 s, ≤ 7.5 s, 5 attempts"**, which no run can
    satisfy: §4.5 floors every one of the five backoffs at the header's 7 s, so the call waits
    28–29 s. The 7–7.5 s band describes a *single* backoff (the first draw is exactly 7.0 s, since
    the jitter term is ≤ 500 ms at n = 1) — the row now says that, and the exhaustion assertion is
    stated separately as "5 retries then `E-PROV-RATELIMIT`".
17. **§15.4's milestone rows are spans over each family's numbering *block*, but §0 says a range
    enumerates every integer as its own case** — so M1 claimed `T-PROV-001..048` (015–019 undefined),
    `T-SESS-010..031` (014–019, 024–029), `T-SEC-001..003` (001 defined nowhere, 003 absent from
    the document) and `T-ARCH-005..010` (009/010 absent): four criteria that could never be met.
    M1's row now spans only ids that have a definition. M0 and M2–M5 overrun the same way
    (`T-TOOL-018..100`, `T-CMD-057..099`, `T-PERM-014..019`, `T-PERF-001..004`, `T-OPS-013..019`, …);
    those are left as blocks to be narrowed by the milestone that executes them, so this entry
    records it rather than a rule that would silently rewrite five rows at once.
18. **§4.8 named its estimator branches "Latin" and "CJK/code-heavy" without saying how to tell
    them apart; §4.9 defined `aliases` without ever saying a `model` value could be one, so
    REQ-PROV-013 compared canonical ids only and rejected `model = "sonnet"`; §4.9's `auth_header`
    is one string holding two shapes (`"Authorization: Bearer"` and Anthropic's bare
    `"x-api-key"`) with no rule for reading them; and REQ-PROV-014 mandated a fallback while
    naming no warning, leaving `W-REG-FALLBACK` registered and tested but emitted by nothing** →
    §4.8 now states the predicate (ASCII and ≥ 80% letters/digits/spaces ⇒ `ceil(chars/4)`, else
    `ceil(bytes/3)`) and what a whole-prompt estimate counts; §4.9 says a `model` MAY be an id or
    an alias resolving *before* limits are read, documents `auth_header` as `name[: prefix]`, and
    REQ-PROV-014/§11.4.2 follow. The registry document itself moved to `cairn-core::registry` —
    §3.2 gives `cairn-provider` `registry` in its MAY list, naming no crate, while both consumers
    can reach `core` — so §3.2's responsibility columns changed and `registry` left that list.
19. **§4.1's `Block::Reasoning.signature` had no producer; §4.3's assembly had no empty-buffer,
    synthetic-id/name, `Usage`-before-`Finish`, keep-alive, or cut-stream rule; §4.4's "Stop on
    tool" row read as request shaping only; §4.2 listed the Responses API with no shaping row**
    → §3.4's `StreamEvent` gains `ReasoningSignature`; §4.3 now states empty-buffer-parses-as-`{}`,
    the `synthetic-{index}`/empty-name rule, `Usage`-when-complete with `Finish`-only-at-end-of-stream,
    blank-`data:`-as-keep-alive, and no-`ToolCallEnd`-no-`Finish` for a cut stream; §4.4's row 6
    reads in both directions (plain stop + tools seen ⇒ `ToolUse`); the Responses API stays listed
    but marked not decoded in M1. Implemented as `cairn-provider::wire::WireDecoder`.
20. **D-04 fixed rustls with bundled Mozilla roots while §15.2 promised native roots; the bundle
    crate's `CDLA-Permissive-2.0` was outside D-13; §4.3 never said how a mid-stream failure
    reaches a `BoxStream` caller**
    → D-04 wins (it is the decision): §15.2 pins `rustls`/`webpki-roots` with `ca_bundle` adding
    a site CA, and the `reqwest` row names its exact feature flags (`rustls-tls-webpki-roots`,
    `stream`, `gzip`, `http2`, no `json` — the body is shaped as a `Value` first). D-13 gains
    `CDLA-Permissive-2.0` (permissive CA-bundle data, no copyleft — the Unicode-3.0 rule).
    §4.3 now ends mid-stream failures with `Finish { stop: Error }`, detail logged per §12.1.
    §4.4 now states the `max_completion_tokens` rule and mandatory `include_usage`; §4.10 states
    keyless providers are `Ready` without a key. Implemented as `cairn-provider::transport`
    plus the five adapters.
21. **T-FAULT-006 demanded "retry once → `E-PROV-PROTO`" — a retryable-then-fatal sequence no
    fault can produce — while §4.7 resolves truncation-as-disconnect with five retries; §3.4's
    `stream` had no channel for a mid-stream fault, so a retry loop cannot apply the matrix
    past the first byte; §3.2's `cairn-provider` MAY row named no timer for REQ-PROV-006's
    cancel-mid-backoff bound; and nothing said what a call that fails before its first byte
    yields**
    → new `Truncated` fault sharing `E-PROV-NET` with one retry (§4.5 row added; T-FAULT-006's
    terminal corrected to NET — the "retry once" was right, the code was not).
    `Provider::take_last_error` (§3.4) carries the fault out, reading clears it. §3.2's MAY row
    gains tokio (time only). A call that never gets its first byte still ends with one terminal
    `Finish { stop: Error }`; cancellation ends silently. Implemented as
    `cairn-provider::retry_loop::stream_with_retry`.
22. **REQ-PROV-009 discards "any partial text" without saying where a partial turn starts in
    a retried stream, and §4.6's extractor classifies blocks without saying who counts the
    bad ones toward the two-repairs budget**
    → §4.7 states the turn boundary (a `MessageStart` after `Finish { stop: Error }` opens the
    new turn). The bad-block counting and session-scoped disable (`E-PROV-FALLBACK`) stay with
    the turn loop, which owns the session — `cairn-provider::fallback` classifies, the loop
    counts. Covered by the mock cassettes and live loopback proofs.

## Known limitations of the M0 delivery

- **CI is green on all three OSes.** `.github/workflows/ci.yml` mirrors §14.7's `lint`, unit
  (ubuntu/macOS/Windows), `coverage` and `msrv` rows; the early M1 pushes were red on
  macOS/Windows/coverage for pre-existing reasons (CRLF schema diff, two POSIX-only path
  tests, `llvm-cov --out`, a macOS compile error in `paths.rs`) and are fixed — six jobs
  green, and every push is verified with the CI watcher before the next chunk lands.
- **Branch coverage is enforced only as LLVM regions.** §14.7 sets both line and branch targets;
  `cargo llvm-cov --branch` is unstable on this toolchain, so the numbers above come from the
  default summary (line / region / function). The CI gate runs `--fail-under-lines` on lines only.
- **T-ARCH-008 exercises a synthetic flush.** `shutdown_with_flush` is tested against an in-memory
  closure; wiring it to the real session writer (and asserting fsync failure there) lands with M1's
  `cairn-session`.
- **`schemas/tools/` is empty** until the M4 tool registry emits its schemas; `check-schemas.sh` only
  notes their absence today.
- **Foreign-target `cargo check` no longer runs locally.** `ring` (via `reqwest` →
  `hyper-rustls`) needs a native C toolchain per target, so the Windows/macOS legs are
  verified by CI runners, not by `cargo check --target` on Linux. New code in this
  area stays portable by construction (no platform APIs, loopback and `temp_dir` in
  tests) and every push is CI-verified before the next chunk lands.
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

The session store, `cairn-sse`, the boxed `Provider` trait, §4.5's retry policy and §4.8/§4.9's
registry + accounting are done, and the spec now agrees with where the provider code belongs.
Next, in order:

1. **The mock provider and its cassettes** are landed (`MockProvider`, five committed
   scripts, live loopback proofs). The headless one-shot is landed too (`run -p` with
   three formats and 0/3/7 exits). What remains on the provider side: T-PROV-002/007/030/031
   need the message assembler (deltas → `ToolCall` blocks with §4.3 repair, caps and orphan
   checks — agent-side, lands with the turn loop), T-PROV-008's counting needs the same
   loop, and T-PROV-027's logged `warn` needs §12.1's logging.
2. **User-model bridging and the compat probe.** Adapters resolve against the bare
   registry: a user-defined `models.<id>` (REQ-PROV-013's escape hatch) and a proxy's
   declared capabilities need the `run` wiring, which owns the full `Config`, to reach
   them — plus §4.2's auto-detect probe (first `stream()` with tools refines
   `tool_calling`) and §4.10 steps 3–5 (keychain, config files). None of it is network
   shape; all of it is construction plumbing.
4. **`cairn-parse`** (the tree-sitter wrapper).
5. **Headless `cairn run -p …`** with text/json/stream-json output and the §11.2 exit codes
   (T-CLI-010..014/016/017), then logging + redaction (§12.1).

Every gate above must stay green as these land.
