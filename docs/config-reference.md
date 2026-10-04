# Configuration reference

Every configuration key Cairn accepts — its type, default, validation rule and its flag/environment
cross-references — is defined by the JSON Schema generated from the Rust config structs. This page
is the human-readable companion: it documents how configuration is layered and merged, how
`CAIRN_*` environment variables map onto keys, and which validation codes you may encounter.

> **Status:** M0 stub — this page is generated from `schemas/config.schema.json` (SPEC §11.4.3);
> the full table lands at M5.

## Layers and precedence

Configuration is assembled from layers, highest precedence first (SPEC §11.5):

1. CLI flags (`--mode`, `-m`, …)
2. `CAIRN_*` environment variables
3. `--config FILE` (explicit, if given) — *treated as the top config layer*
4. Project config `.cairn/config.toml` (workspace root)
5. Project config `.cairn/config.toml` in parent dirs (nearest wins over farther)
6. User config `~/.config/cairn/config.toml` (XDG; macOS/Windows equivalents SPEC §11.6)
7. System config `/etc/cairn/config.toml` (`$XDG_CONFIG_DIRS` first existing)
8. Built-in defaults (SPEC §11.4.1)

The effective configuration is printable with `cairn config list --effective --json`, each key
annotated with its winning layer (`source: "flag|env|project|user|system|default"`) — REQ-CLI-006.
A worked example of the whole pipeline is SPEC §11.8 (test T-CFG-010).

## Merge semantics

| Value kind | Merge rule | Example |
|-----------|-----------|---------|
| Scalar | higher layer replaces | `mode` |
| Map / TOML table (`[ui]`, `[providers.x]`) | key-wise recursive merge; absent keys inherit | `[ui]` from project keeps user's `theme` |
| Array of tables (`[[hooks]]`, `[[mcp.servers]]`) | **replace entirely** if the layer defines any; matched by `name`/`command` for diagnostics only | project `[[]]` hides user hooks |
| Array of scalars (`env_allowlist`, `include`, `exclude`, `redact_patterns`, `verify.commands`) | config key `list_merge = "replace"` by default; if the layer sets `key += [...]` (TOML append syntax) → **append and dedup (order preserved)** | `discovery.exclude += ["docs/**"]` |
| Keys marked `+=` semantics | supported for all list keys; documented as Cairn's only divergence from plain TOML | — |
| Env var for a list | splits on `:` (paths) or `,` (others); empty string → empty list | `CAIRN_VERIFY_COMMANDS="a,b"` |
| Profile (`--profile X`) | loads `~/.config/cairn/profiles/X.toml` as an additional layer *above* user config, below project | — |
| Invalid higher-layer value | **falls back to next lower layer** with `W-CFG-FALLBACK` (never adopt a broken value) | REQ-CLI-005 |

## Environment variables

SPEC §11.3 defines the mapping rules:

- **Namespace:** `CAIRN_*` only, plus the standard `NO_COLOR`, `CLICOLOR_FORCE`, `TERM`, `EDITOR`,
  `PAGER`, `BROWSER`, `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`, and the provider key variables from
  SPEC §4.10 (`CAIRN_<PROVIDER>_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …).
- **Mapping:** each variable names a config key (`CAIRN_MODEL` → `model`, `CAIRN_MODE` → `mode`,
  `CAIRN_LOG_LEVEL`/`CAIRN_LOG_FILE` → `log.*`, …). The complete machine-readable mapping ships
  in the schema as `x-env-var` / `x-flag` / `x-config-key` annotations for every flag of
  SPEC §11.1 — print it with:

  ```sh
  cairn config list --json-schema
  ```

- **Booleans:** `1,true,yes,on` → true; `0,false,no,""` → false; anything else → `E-CFG-BADENV`
  (exit 2).
- **Lists:** split on `:` (path lists) or `,` (others); an empty string produces an empty list.
- **Overrides:** `CAIRN_CONFIG` (extra config file), `CAIRN_HOME` (replaces config+data+cache
  roots — REQ-CLI-007), `CAIRN_CACHE_DIR` / `CAIRN_DATA_DIR`.

## Validation and error codes

`cairn config validate` reports **all** errors at once (not fail-fast), with `file:line` when
available from the TOML span map — REQ-CLI-003. A config file that fails validation does not
prevent startup when the broken sections are not needed by the running command; such errors are
shown as warnings `W-CFG-PARTIAL`. **Errors in `providers`, `model` and `security` are fatal
(exit 2)** — REQ-CLI-004.

Validation rules beyond types, and their exact codes (SPEC §11.4.2):

| Rule | Code |
|------|------|
| All enums strictly validated; unknown value lists valid options | `E-CFG-BADVALUE` |
| Percent groups (`context.*_pct`) need not sum ≤ 100 but `output_reserve_pct` must be ≥ 5 (REQ-CTX-010) | `E-CFG-RANGE` |
| `weights.page + weights.bm25 == 1.0 ± 0.001` | `E-CFG-SUM` |
| `personalization.*` sums to 1.0 ± 0.001 | `E-CFG-SUM` |
| Regex fields compiled at load (`redact_patterns`, permission `command_regex`) | `E-CFG-BADREGEX` |
| Glob fields parsed by globset | `E-CFG-BADGLOB` |
| `model` resolves in registry or has explicit `[models."id"]` | `E-CFG-NOMODEL` (REQ-PROV-013) |
| Unknown top-level/known-section keys rejected (strict) unless `--allow-unknown-keys` | `E-CFG-UNKNOWN` |
| `schema_version` must be 1 (else migration, SPEC §11.7) | `E-CFG-VERSION` |
| `mode = auto_unsafe` requires `modes.allow_unsafe` (G-M1) | `E-CFG-UNSAFEBLOCKED` |
| `log.redact = false` requires `trace.debug_unsafe = true` (§12.1) | `E-CFG-UNSAFEREDACT` |
| Duplicate `[[mcp.servers]].name`, `[[custom_tools]].name` | `E-CFG-DUPNAME` |
| Path fields must be absolute or `~`-prefixed | `E-CFG-BADPATH` |

Related codes defined outside §11.4.2:

- `E-CFG-BADENV` — invalid boolean value in a `CAIRN_*` environment variable (SPEC §11.3, exit 2).
- `W-CFG-PARTIAL` — validation errors in sections not needed by the running command
  (REQ-CLI-004).
- `W-CFG-FALLBACK` — invalid higher-layer value fell back to the next lower layer
  (REQ-CLI-005).
- `E-CFG-THEME` — unknown keys in a theme file (SPEC §10.5).
- `E-CFG-KEYCONFLICT` / `E-CFG-KEYRESERVED` — keybinding conflicts and reserved OS-level chords
  (SPEC §10.4).

## Key index

The full enumeration of keys — every key from SPEC §11.4.1 with type, default, validation rule and
its flag/environment cross-reference — is **generated from the JSON Schema at M5** (SPEC §15.6,
drift-checked against `schemas/config.schema.json`). Until then, the normative key list with
defaults is SPEC §11.4.1, and the machine-readable form is:

```sh
cairn config list --json-schema
```
