//! JSON Schema generation with CLI flag annotations, and the `--effective` view
//! (SPEC §11.4.3, REQ-CLI-006, T-CFG-001).

use crate::issue::Source;
use crate::load::Loaded;
use crate::model::Config;
use serde::Serialize;

/// One row of the §11.1 flag tables.
#[derive(Debug, Clone, Serialize)]
pub struct FlagSpec {
    /// Flag as written on the command line.
    pub flag: &'static str,
    /// Environment variable ("" when the spec says `—`).
    pub env: &'static str,
    /// Dotted config key ("" when the spec says `—`).
    pub config_key: &'static str,
    /// Default as documented in §11.1.
    pub default: &'static str,
    pub description: &'static str,
    /// `true` for `run`-specific flags.
    pub run_flag: bool,
}

/// Every flag in SPEC §11.1 (global table + `run` table). T-CFG-001 asserts each
/// row has `x-flag` / `x-env-var` / `x-config-key` annotations in the schema.
pub const FLAGS: &[FlagSpec] = &[
    // --- global ---
    FlagSpec {
        flag: "--workspace",
        env: "CAIRN_WORKSPACE",
        config_key: "",
        default: "git root or CWD",
        description: "workspace root",
        run_flag: false,
    },
    FlagSpec {
        flag: "-m, --model",
        env: "CAIRN_MODEL",
        config_key: "model",
        default: "anthropic/claude-sonnet-4-5",
        description: "model id",
        run_flag: false,
    },
    FlagSpec {
        flag: "--mode",
        env: "CAIRN_MODE",
        config_key: "mode",
        default: "build",
        description: "operating mode",
        run_flag: false,
    },
    FlagSpec {
        flag: "-c, --config",
        env: "CAIRN_CONFIG",
        config_key: "",
        default: "",
        description: "additional config file (highest config precedence)",
        run_flag: false,
    },
    FlagSpec {
        flag: "--profile",
        env: "CAIRN_PROFILE",
        config_key: "profile",
        default: "default",
        description: "config profile selector",
        run_flag: false,
    },
    FlagSpec {
        flag: "--output",
        env: "CAIRN_OUTPUT",
        config_key: "output.format",
        default: "text if not TTY, else tui",
        description: "output format",
        run_flag: false,
    },
    FlagSpec {
        flag: "--no-color",
        env: "NO_COLOR",
        config_key: "ui.color",
        default: "color if TTY",
        description: "disable ANSI",
        run_flag: false,
    },
    FlagSpec {
        flag: "--log-level",
        env: "CAIRN_LOG_LEVEL",
        config_key: "log.level",
        default: "warn",
        description: "log verbosity",
        run_flag: false,
    },
    FlagSpec {
        flag: "--log-file",
        env: "CAIRN_LOG_FILE",
        config_key: "log.file",
        default: "~/.local/share/cairn/logs/cairn.log",
        description: "log destination",
        run_flag: false,
    },
    FlagSpec {
        flag: "--trace",
        env: "CAIRN_TRACE",
        config_key: "trace.enabled",
        default: "false",
        description: "record full model I/O",
        run_flag: false,
    },
    FlagSpec {
        flag: "--dangerously-skip-permissions",
        env: "",
        config_key: "modes.allow_unsafe",
        default: "false",
        description: "enables auto-unsafe (G-M1)",
        run_flag: false,
    },
    FlagSpec {
        flag: "--no-update-check",
        env: "CAIRN_NO_UPDATE_CHECK",
        config_key: "update.check",
        default: "false",
        description: "skip background update check",
        run_flag: false,
    },
    FlagSpec {
        flag: "--offline",
        env: "CAIRN_OFFLINE",
        config_key: "network.offline",
        default: "false",
        description: "no network; provider calls fail fast",
        run_flag: false,
    },
    FlagSpec {
        flag: "-q, --quiet",
        env: "CAIRN_QUIET",
        config_key: "ui.quiet",
        default: "false",
        description: "suppress progress on stderr",
        run_flag: false,
    },
    FlagSpec {
        flag: "-v, --verbose",
        env: "CAIRN_VERBOSE",
        config_key: "",
        default: "false",
        description: "-v info, -vv debug, -vvv trace",
        run_flag: false,
    },
    FlagSpec {
        flag: "--version",
        env: "",
        config_key: "",
        default: "",
        description: "print version and exit 0",
        run_flag: false,
    },
    FlagSpec {
        flag: "-h, --help",
        env: "",
        config_key: "",
        default: "",
        description: "help for command, exit 0",
        run_flag: false,
    },
    // --- run ---
    FlagSpec {
        flag: "-p, --prompt",
        env: "",
        config_key: "",
        default: "",
        description: "prompt text",
        run_flag: true,
    },
    FlagSpec {
        flag: "--prompt-file",
        env: "",
        config_key: "",
        default: "",
        description: "read prompt from file (- = stdin)",
        run_flag: true,
    },
    FlagSpec {
        flag: "--stdin",
        env: "",
        config_key: "",
        default: "false",
        description: "read prompt from stdin",
        run_flag: true,
    },
    FlagSpec {
        flag: "--input",
        env: "",
        config_key: "",
        default: "",
        description: "JSONL transcript to preload",
        run_flag: true,
    },
    FlagSpec {
        flag: "--session",
        env: "",
        config_key: "",
        default: "new session",
        description: "continue an existing session",
        run_flag: true,
    },
    FlagSpec {
        flag: "--approve-plan",
        env: "",
        config_key: "plans.auto_approve",
        default: "false",
        description: "auto-approve a produced plan (headless)",
        run_flag: true,
    },
    FlagSpec {
        flag: "--max-iterations",
        env: "CAIRN_MAX_ITERATIONS",
        config_key: "auto.max_iterations",
        default: "per mode",
        description: "override loop cap",
        run_flag: true,
    },
    FlagSpec {
        flag: "--allow-ask",
        env: "",
        config_key: "",
        default: "false",
        description: "read approval answers from stdin as JSON lines",
        run_flag: true,
    },
    FlagSpec {
        flag: "--input-fmt",
        env: "",
        config_key: "",
        default: "text",
        description: "format of --input",
        run_flag: true,
    },
    FlagSpec {
        flag: "--tee",
        env: "",
        config_key: "",
        default: "false",
        description: "also print progress to stderr",
        run_flag: true,
    },
    // --- config command ---
    FlagSpec {
        flag: "--json-schema",
        env: "",
        config_key: "",
        default: "",
        description: "print the JSON Schema of config.toml",
        run_flag: false,
    },
    FlagSpec {
        flag: "--effective",
        env: "",
        config_key: "",
        default: "",
        description: "annotate each key with its winning layer",
        run_flag: false,
    },
    FlagSpec {
        flag: "--allow-unknown-keys",
        env: "",
        config_key: "",
        default: "false",
        description: "downgrade E-CFG-UNKNOWN to a warning",
        run_flag: false,
    },
];

/// Generate the JSON Schema for `config.toml` with §11.1 annotations
/// (REQ-ARCH-011, REQ-CLI-006, T-CFG-001).
#[must_use]
pub fn json_schema() -> serde_json::Value {
    let schema = schemars::schema_for!(Config);
    let mut v = serde_json::to_value(schema).expect("schema serializes");
    // SPEC §11.4.3: `default` for every key, derived from `Config::default()`.
    let defaults = serde_json::to_value(Config::default()).expect("defaults serialize");
    inject_defaults(&mut v, &defaults, &mut Vec::new());
    inject_patterns(&mut v);
    let obj = v.as_object_mut().expect("schema is an object");
    // SPEC §11.4.3: x-flag / x-env-var / x-config-key for every §11.1 row.
    let rows: Vec<serde_json::Value> = FLAGS
        .iter()
        .map(|f| {
            serde_json::json!({
                "x-flag": f.flag,
                "x-env-var": f.env,
                "x-config-key": f.config_key,
                "default": f.default,
                "description": f.description,
                "x-scope": if f.run_flag { "run" } else { "global" },
            })
        })
        .collect();
    obj.insert("x-cli-flags".to_string(), serde_json::Value::Array(rows));
    obj.insert(
        "x-env-var-map".to_string(),
        serde_json::to_value(
            crate::load::ENV_KEYS
                .iter()
                .map(|(var, key, _)| serde_json::json!({ "env": var, "config_key": key }))
                .collect::<Vec<_>>(),
        )
        .expect("env map serializes"),
    );
    v
}

fn json_pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// JSON pointer of `path` inside the schema, resolving `$ref` hops to
/// `#/definitions/…` (schemars 0.8 emits refs for named structs).
fn property_pointer(root: &serde_json::Value, path: &[String]) -> Option<String> {
    let mut ptr = String::new();
    for seg in path {
        let mut node = root.pointer(&ptr)?;
        // schemars 0.8 emits either a bare `$ref` or `allOf: [{ $ref }]`.
        let r = node
            .get("$ref")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                node.get("allOf")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|first| first.get("$ref"))
                    .and_then(serde_json::Value::as_str)
            });
        if let Some(r) = r {
            let name = r.rsplit('/').next()?;
            ptr = format!("/definitions/{}", json_pointer_escape(name));
            node = root.pointer(&ptr)?;
            if node.is_null() {
                return None;
            }
        }
        ptr.push_str("/properties/");
        ptr.push_str(&json_pointer_escape(seg));
    }
    // The final node may itself be the `$ref` (nothing is descended into after it).
    for _ in 0..4 {
        let node = root.pointer(&ptr)?;
        let r = node
            .get("$ref")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                node.get("allOf")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|first| first.get("$ref"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string);
        match r {
            Some(r) => {
                let name = r.rsplit('/').next()?.to_string();
                let target = format!("/definitions/{}", json_pointer_escape(&name));
                root.pointer(&target)?;
                ptr = target;
            }
            None => break,
        }
    }
    root.pointer(&ptr).map(|_| ptr)
}

/// `pattern` annotations for id-like strings (SPEC §11.4.3).
const PATTERN_KEYS: &[(&str, &str)] = &[
    ("profile", "^[a-z0-9_-]{1,32}$"),
    ("custom_tools.name", "^[a-z][a-z0-9_]{1,63}$"),
];

fn inject_patterns(root: &mut serde_json::Value) {
    for (key, pattern) in PATTERN_KEYS {
        let path: Vec<String> = key.split('.').map(str::to_string).collect();
        if let Some(ptr) = property_pointer(root, &path) {
            if let Some(node) = root.pointer_mut(&ptr) {
                node["pattern"] = serde_json::Value::String((*pattern).to_string());
            }
        }
    }
}

/// Stamp `default` onto every property that has a value in `sample`
/// (SPEC §11.4.3: `default` for every key).
fn inject_defaults(
    root: &mut serde_json::Value,
    sample: &serde_json::Value,
    path: &mut Vec<String>,
) {
    let serde_json::Value::Object(map) = sample else {
        return;
    };
    for (key, value) in map {
        path.push(key.clone());
        if let Some(ptr) = property_pointer(root, path) {
            if let Some(node) = root.pointer_mut(&ptr) {
                node["default"] = value.clone();
                let next = path.clone();
                inject_defaults(root, value, &mut { next });
            }
        }
        path.pop();
    }
}

/// Pretty-printed schema (what `cairn config list --json-schema` prints).
#[must_use]
pub fn json_schema_pretty() -> String {
    serde_json::to_string_pretty(&json_schema()).expect("schema serializes")
}

/// One effective key with its winning layer (REQ-CLI-006).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectiveEntry {
    pub path: String,
    pub value: serde_json::Value,
    /// `default|system|user|profile|project|explicit|env|flag`.
    pub source: &'static str,
    /// Layer detail: env var name, flag name or profile name.
    pub detail: Option<String>,
}

/// Flatten the effective config into `path -> (value, source)` entries.
#[must_use]
pub fn effective(loaded: &Loaded) -> Vec<EffectiveEntry> {
    let value = serde_json::to_value(&loaded.config).expect("config serializes");
    let mut paths: Vec<String> = Vec::new();
    collect_paths(&value, "", &mut paths);
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let v = pointer(&value, &path);
            let source = loaded.sources.get(&path);
            EffectiveEntry {
                value: v,
                source: source.map_or("default", Source::as_str),
                detail: source.and_then(Source::detail).map(str::to_string),
                path,
            }
        })
        .collect()
}

fn collect_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => out.push(prefix.to_string()),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_paths(v, &p, out);
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

fn pointer(root: &serde_json::Value, dotted: &str) -> serde_json::Value {
    let mut cur = root;
    for seg in dotted.split('.') {
        match cur.get(seg) {
            Some(next) => cur = next,
            None => return serde_json::Value::Null,
        }
    }
    cur.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{load, FlagOverrides, LoadOptions};
    use std::collections::BTreeMap;

    /// T-CFG-001: every §11.1 flag carries x-flag / x-env-var / x-config-key.
    #[test]
    fn every_spec_flag_is_annotated() {
        let schema = json_schema();
        let flags = schema["x-cli-flags"].as_array().expect("x-cli-flags array");
        assert!(
            flags.len() >= 27,
            "expected the §11.1 rows, got {}",
            flags.len()
        );
        for row in flags {
            for key in ["x-flag", "x-env-var", "x-config-key"] {
                assert!(
                    row.get(key).is_some_and(serde_json::Value::is_string),
                    "missing {key} annotation on {row}"
                );
            }
            assert!(
                row["x-flag"].as_str().is_some_and(|s| !s.is_empty()),
                "{row}"
            );
        }
        let spec_rows = [
            "--workspace",
            "-m, --model",
            "--mode",
            "-c, --config",
            "--profile",
            "--output",
            "--no-color",
            "--log-level",
            "--log-file",
            "--trace",
            "--dangerously-skip-permissions",
            "--no-update-check",
            "--offline",
            "-q, --quiet",
            "-v, --verbose",
            "--version",
            "-h, --help",
            "-p, --prompt",
            "--prompt-file",
            "--stdin",
            "--input",
            "--session",
            "--approve-plan",
            "--max-iterations",
            "--allow-ask",
            "--input-fmt",
            "--tee",
        ];
        for row in spec_rows {
            assert!(
                FLAGS.iter().any(|f| f.flag == row),
                "§11.1 flag {row} missing from the annotation table"
            );
        }
    }

    /// T-CFG-001b: every annotated env var is either a known CAIRN_* var or a
    /// documented standard variable.
    #[test]
    fn annotated_env_vars_are_known() {
        let known: Vec<&str> = crate::load::ENV_KEYS.iter().map(|(v, _, _)| *v).collect();
        for f in FLAGS {
            if f.env.is_empty() {
                continue;
            }
            let ok = known.contains(&f.env)
                || matches!(
                    f.env,
                    "NO_COLOR" | "CAIRN_WORKSPACE" | "CAIRN_CONFIG" | "CAIRN_VERBOSE"
                );
            assert!(ok, "flag {} references unknown env var {}", f.flag, f.env);
        }
    }

    /// T-CFG-002: schema contains defaults, enums and vendor annotations.
    #[test]
    fn schema_has_defaults_and_enums() {
        let s = json_schema();
        let def = |dotted: &str| -> serde_json::Value {
            let path: Vec<String> = dotted.split('.').map(str::to_string).collect();
            let ptr =
                property_pointer(&s, &path).unwrap_or_else(|| panic!("no pointer for {dotted}"));
            s.pointer(&ptr)
                .and_then(|n| n.get("default").cloned())
                .unwrap_or(serde_json::Value::Null)
        };
        assert_eq!(def("temperature"), serde_json::json!(0.2));
        assert_eq!(def("mode"), serde_json::json!("build"));
        assert_eq!(def("schema_version"), serde_json::json!(1));
        assert_eq!(def("ui.frame_rate"), serde_json::json!(60));
        assert_eq!(def("ui.theme"), serde_json::json!("cairn-dark"));
        assert_eq!(def("shell.kill_grace_ms"), serde_json::json!(2000));
        assert_eq!(def("repo_map.weights.page"), serde_json::json!(0.55));
        assert_eq!(def("log.redact"), serde_json::json!(true));
        assert_eq!(def("mcp.servers"), serde_json::json!([]));
        assert_eq!(def("modes.allow_unsafe"), serde_json::json!(false));
        assert_eq!(
            def("permissions.ask_timeout_ms"),
            serde_json::json!(600_000)
        );
        // `$ref`-aware lookup: the enum lives on the definition, not the property.
        let node = |dotted: &str| -> serde_json::Value {
            let path: Vec<String> = dotted.split('.').map(str::to_string).collect();
            let ptr =
                property_pointer(&s, &path).unwrap_or_else(|| panic!("no pointer for {dotted}"));
            s.pointer(&ptr).cloned().unwrap_or(serde_json::Value::Null)
        };
        let names = |dotted: &str| -> Vec<String> {
            let n = node(dotted);
            if let Some(list) = n["enum"].as_array() {
                return list
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
            // documented variants: `oneOf: [{ enum: [v] }, …]`
            n["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{dotted} enum"))
                .iter()
                .filter_map(|v| v["enum"].as_array())
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        };
        assert_eq!(names("mode"), vec!["plan", "build", "auto", "auto-unsafe"]);
        assert!(
            node("log.level")["enum"].as_array().is_some(),
            "log.level enum"
        );
        assert_eq!(
            names("output.format"),
            vec!["text", "json", "stream-json", "tui"]
        );
        assert_eq!(names("ui.diff_layout"), vec!["auto", "inline", "side"]);
        // pattern annotations (SPEC §11.4.3)
        assert_eq!(
            node("profile")["pattern"],
            serde_json::json!("^[a-z0-9_-]{1,32}$")
        );
        assert!(s.get("x-cli-flags").is_some());
        assert!(s.get("$schema").is_some(), "draft 2020-12 marker");
    }

    /// T-SCHEMA-002: committed schema matches the structs (drift detection).
    #[test]
    fn committed_schema_matches_structs() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../schemas/config.schema.json");
        if !path.exists() {
            // Generated on first `cargo run -p cairn-cli -- config list --json-schema`.
            return;
        }
        let committed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read schema"))
                .expect("parse schema");
        assert_eq!(
            committed,
            json_schema(),
            "schemas/config.schema.json is stale — regenerate with `cairn config list --json-schema > schemas/config.schema.json`"
        );
    }

    #[test]
    fn effective_covers_nested_and_array_keys() {
        let mut env = BTreeMap::new();
        env.insert("CAIRN_QUIET".to_string(), "1".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            flags: FlagOverrides {
                model: Some("openai/o4-mini".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        let eff = effective(&loaded);
        let find = |p: &str| {
            eff.iter()
                .find(|e| e.path == p)
                .unwrap_or_else(|| panic!("missing {p}"))
        };
        assert_eq!(find("model").source, "flag");
        assert_eq!(find("model").value, "openai/o4-mini");
        assert_eq!(find("ui.quiet").source, "env");
        assert_eq!(find("ui.quiet").value, true);
        assert_eq!(find("shell.env_allowlist").source, "default");
        assert!(find("shell.env_allowlist").value.is_array());
        assert_eq!(find("todo").source, "default");
        assert_eq!(find("providers.anthropic.max_retries").value, 5);
    }
}
