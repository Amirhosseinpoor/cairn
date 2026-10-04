# Cairn

Cairn is a terminal-native AI coding agent: a fast, safe, scriptable assistant that lives in your
shell and works directly on your repository.

![build](https://img.shields.io/github/actions/workflow/status/cairn-dev/cairn/ci.yml?branch=main&label=build)
![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![MSRV](https://img.shields.io/badge/MSRV-1.83-blue.svg)
![status](https://img.shields.io/badge/status-milestone%20M0-orange)

## What it does

- **Explained edits** — describe a change; Cairn locates the files, edits them with fuzzy
  matching, validates syntax, runs your tests and reports the diff (SPEC §1.2, UC-1).
- **Plans before code** — Plan mode explores a repository read-only and emits a structured plan
  artifact with steps, risks and a test strategy; Build and Auto modes execute an approved plan
  behind approval gates and guardrails (UC-2, UC-3).
- **Repository Q&A** — ask questions about the code using the repo map plus `grep`/`read_file`,
  without modifying anything (UC-4).
- **Built for scripts and CI** — headless `cairn run -p "…"` with JSON or stream-JSON output and
  deterministic exit codes (UC-5).
- **Safe by default** — permission gating on every tool, AST-based shell analysis, OS sandboxing,
  secret redaction before write, and per-turn checkpoints with `/undo` (SPEC §9).

## Install

Cairn is built in Rust. The toolchain file asks for **stable** (plus `rustfmt` and `clippy`);
**1.83** is the declared minimum supported version, held by `rust-version` in every `Cargo.toml`
and by the `msrv` CI job (SPEC §15.1, ADR-0001).

From a checkout of this repository:

```sh
cargo install --path .
```

or build the release binary directly:

```sh
cargo build --release --locked
./target/release/cairn --version
```

Release CI additionally produces a static Linux musl artifact; SPEC §2.2 caps the stripped musl
binary at ≤ 18 MB and `cairn --version` at ≤ 40 ms. Other channels (install script, Homebrew,
signed GitHub Releases) are specified in SPEC §1.5 and arrive with the release milestone (M5).

## Examples

```sh
# Single-turn headless run: one JSON object per event on stdout, stable exit code
cairn run -p "Fix the failing test in tests/parser_test.py" --output json
```

```sh
# Inspect the effective configuration and the layer each key came from
cairn config list --effective
```

```sh
# Diagnose an installation: config, provider keys, shell, git, sandbox, grammars, sessions
cairn doctor
```

## Where things live

| Path | Contents |
|------|----------|
| `~/.config/cairn/config.toml` | user configuration (XDG on Linux; macOS/Windows equivalents in SPEC §11.6) |
| `~/.local/share/cairn/sessions/` | session files, `<workspace_hash>/<session_id>.jsonl` |
| `~/.cache/cairn/` | index database, compiled grammars, traces |
| `.cairn/` | workspace directory: `config.toml`, `permissions.json`, `plans/`, `todos.json`, `subagents/` |
| `.cairnignore` | project ignore file |
| `AGENTS.md` | project instructions (`CAIRN.md` accepted as an alias — SPEC §5.7) |

Logs default to `~/.local/state/cairn/logs/` (SPEC §11.6).

## Documentation

- [Specification](docs/spec.md) — the normative source for everything Cairn does
- [Configuration reference](docs/config-reference.md) — M0 stub, complete at M5
- [ADRs](docs/adr/README.md) — one per decision in SPEC §2

Planned per SPEC §15.6: `docs/contributor-guide.md` (M3), `docs/security.md` (M4),
`docs/user-guide.md` (M5).

## Status

Development follows the milestones in SPEC §15.4. Cairn is currently at **M0 — Skeleton &
contracts**: the workspace, config loader (layering, validation, JSON Schema), CLI command tree,
event bus and the stable error-code registry are in place. Providers, tools, sessions, the context
engine, the TUI and the sandbox land in M1–M5 — see [PROGRESS.md](PROGRESS.md) for the live
picture. Until their milestone ships, commands such as `cairn run`, `cairn chat`, `cairn resume`,
`cairn export` and `cairn update` parse and validate their flags, then exit `1` with
`E-IMPL-STAGE` naming the milestone that delivers them.

## License

Apache-2.0 (SPEC §2.1, D-14): source available, with a commercial license sold separately.
