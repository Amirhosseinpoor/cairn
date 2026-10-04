# Changelog

All notable changes to this project will be documented in this file.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Per SPEC §15.6, changes to `schema_version` (config, session, event-envelope versions) and any
change to the stable exit-code table (SPEC §11.2) are called out explicitly in the entry where they
happen.

There are no released versions yet — only `[Unreleased]` exists until the first tag.

## [Unreleased]

### Added

- **Workspace skeleton** — Cargo workspace with all 19 crates stubbed, shared lint/format policy
  (`clippy.toml`, `rustfmt.toml`), pinned toolchain (stable 1.83.0), committed lockfile, and
  `cargo-deny` configuration (`deny.toml`, D-13 license allow-list).
- **Configuration** — `cairn-config`: layered loader with the 8-level precedence of SPEC §11.5,
  validation rules and `E-CFG-*` codes (§11.4.2), `cairn config list --effective` with per-key
  source annotations, and JSON Schema emission with `x-env-var` / `x-flag` / `x-config-key`
  annotations for every §11.1 flag (§11.4.3).
- **CLI command tree** — `cairn-cli`: full §11.1 subcommand and flag surface; `version`, `config`,
  `auth`, `mcp`, `sessions` and `doctor` are implemented, while `run`, `chat`, `resume`, `export`
  and `update` validate their flags and exit `1` with `E-IMPL-STAGE` until their milestone lands
  (interim behavior, removed as milestones ship).
- **Event bus and schemas** — typed events with critical/droppable lanes (REQ-ARCH-005/006) and
  generated event JSON Schemas under `schemas/events/`.
- **Error-code registry** — `cairn-core`: the 153 stable `E-*`/`W-*` codes of SPEC §16.1, exit
  statuses (§11.2), ids, modes and the redactor.
- **Documentation** — `ADR-0001`..`ADR-0020` (one per decision D-01..D-18 plus the checkpoint
  mechanism of §9.8 and the SSE-resume policy of §4.7), the ADR index, `README.md`, this
  changelog, and the `docs/config-reference.md` stub.

### Changed

- Nothing yet — the first release has not been cut.

### Fixed

- Nothing yet.

### Removed

- Nothing yet.

### Security

- The redactor (`cairn-core`) applies redact-before-write semantics (REQ-SAFE-010) from the start,
  so logs, events and sessions never see unredacted key material once those sinks land.
- Config validation enforces the secret-handling guards of §11.4.2 up front: `log.redact = false`
  is rejected unless `trace.debug_unsafe = true` (`E-CFG-UNSAFEREDACT`), and unsafe modes require
  an explicit opt-in (`E-CFG-UNSAFEBLOCKED`).
