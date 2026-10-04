//! `cairn mcp <sub>` (SPEC §11.1, REQ-TOOL-025).
//!
//! `list` reads the effective config; `add`/`remove` edit one TOML document in
//! place. Connecting to servers (and therefore `inspect`/`refresh`) is M4.

use crate::args::McpCmd;
use crate::commands::config::{
    check_config, read_config, target_file, toml_parse_fail, write_config,
};
use crate::commands::Startup;
use crate::output::Fail;
use cairn_core::error::codes;
use serde_json::json;
use std::path::Path;

/// MCP transports (SPEC §6.6).
const TRANSPORTS: &[&str] = &["stdio", "http"];

fn item_name(table: &toml_edit::Table) -> Option<&str> {
    table
        .get("name")
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_str)
}

fn transport_name(t: cairn_config::model::McpTransport) -> &'static str {
    match t {
        cairn_config::model::McpTransport::Stdio => "stdio",
        cairn_config::model::McpTransport::Http => "http",
    }
}

pub fn run(cmd: &McpCmd, startup: &Startup) -> Result<i32, Fail> {
    match cmd {
        McpCmd::List { json } => list(*json, startup),
        McpCmd::Add {
            name,
            transport,
            command,
            args,
            url,
            cwd,
            project,
        } => add(
            startup,
            name,
            transport,
            command.as_deref().unwrap_or_default(),
            args,
            url.as_deref().unwrap_or_default(),
            cwd.as_deref().unwrap_or_default(),
            *project,
        ),
        McpCmd::Remove { name, project } => remove(startup, name, *project),
        McpCmd::Inspect { name, .. } => Err(Fail::not_implemented(
            &format!("`cairn mcp inspect {name}`"),
            "M4",
        )),
        McpCmd::Refresh { .. } => Err(Fail::not_implemented("`cairn mcp refresh`", "M4")),
    }
}

// --------------------------------------------------------------------- list

fn list(json: bool, startup: &Startup) -> Result<i32, Fail> {
    let servers = &startup.loaded.config.mcp.servers;
    if json {
        let rows: Vec<serde_json::Value> = servers
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "transport": s.transport,
                    "command": s.command,
                    "args": s.args,
                    "url": s.url,
                    // No connection is made until M4.
                    "tools": serde_json::Value::Null,
                    "status": "not_connected",
                    "last_error": serde_json::Value::Null,
                })
            })
            .collect();
        say!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(rows)).expect("mcp rows")
        );
        return Ok(0);
    }
    if servers.is_empty() {
        say!("no MCP servers configured");
        say!(
            "hint: cairn mcp add docs --transport stdio --command npx --arg '-y' --arg 'mcp-docs'"
        );
        return Ok(0);
    }
    for s in servers {
        let detail = if s.command.is_empty() {
            s.url.clone()
        } else {
            format!("{} {}", s.command, s.args.join(" "))
        };
        say!(
            "{:<16} {:<6} {}  status=not_connected (M4)  tools=-  last_error=-",
            s.name,
            transport_name(s.transport),
            detail
        );
    }
    Ok(0)
}

// --------------------------------------------------------------------- edit

#[allow(clippy::too_many_arguments)]
fn add(
    startup: &Startup,
    name: &str,
    transport: &str,
    command: &str,
    args: &[String],
    url: &str,
    cwd: &str,
    project: bool,
) -> Result<i32, Fail> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(Fail::usage(
            format!("invalid server name `{name}`"),
            "use ASCII letters, digits, `_` and `-`".to_string(),
        ));
    }
    if !TRANSPORTS.contains(&transport) {
        return Err(Fail::usage(
            format!("unknown transport `{transport}`"),
            format!("valid transports: {}", TRANSPORTS.join(", ")),
        ));
    }
    if transport == "stdio" && command.is_empty() {
        return Err(Fail::usage(
            "`--transport stdio` needs `--command`",
            "example: `cairn mcp add docs --transport stdio --command npx --arg -y`".to_string(),
        ));
    }
    if transport == "http" && url.is_empty() {
        return Err(Fail::usage(
            "`--transport http` needs `--url`",
            "example: `cairn mcp add remote --transport http --url https://example.com/mcp`"
                .to_string(),
        ));
    }
    if !startup
        .loaded
        .config
        .mcp
        .servers
        .iter()
        .all(|s| s.name != name)
    {
        return Err(dup_name(name));
    }

    let file = target_file(startup, project);
    let old = read_config(&file);
    let mut doc: toml_edit::DocumentMut = old.parse().map_err(|e: toml_edit::TomlError| {
        toml_parse_fail(
            &file,
            e.to_string()
                .lines()
                .next()
                .unwrap_or("invalid TOML")
                .trim(),
        )
    })?;

    let mcp = doc.as_table_mut().entry("mcp").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let mcp_table = mcp.as_table_mut().ok_or_else(|| {
        Fail::new(
            codes::CFG_BADVALUE,
            cairn_core::error::ExitStatus::Usage,
            format!("`mcp` in {} is not a table", file.display()),
            "remove the offending `mcp` entry by hand".to_string(),
        )
    })?;
    let servers = mcp_table
        .entry("servers")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let servers = servers.as_array_of_tables_mut().ok_or_else(|| {
        Fail::new(
            codes::CFG_BADVALUE,
            cairn_core::error::ExitStatus::Usage,
            format!(
                "`mcp.servers` in {} is not an array of tables",
                file.display()
            ),
            "use `[[mcp.servers]]` sections by hand".to_string(),
        )
    })?;
    for existing in servers.iter() {
        if item_name(existing) == Some(name) {
            return Err(dup_name(name));
        }
    }

    let mut table = toml_edit::Table::new();
    table.insert("name", toml_edit::Item::Value(toml_edit::Value::from(name)));
    table.insert(
        "transport",
        toml_edit::Item::Value(toml_edit::Value::from(transport)),
    );
    if transport == "stdio" {
        table.insert(
            "command",
            toml_edit::Item::Value(toml_edit::Value::from(command)),
        );
        let mut list = toml_edit::Array::new();
        for a in args {
            list.push(toml_edit::Value::from(a.as_str()));
        }
        table.insert("args", toml_edit::Item::Value(list.into()));
        if !cwd.is_empty() {
            table.insert("cwd", toml_edit::Item::Value(toml_edit::Value::from(cwd)));
        }
    } else {
        table.insert("url", toml_edit::Item::Value(toml_edit::Value::from(url)));
    }
    servers.push(table);

    let text = doc.to_string();
    check_config(
        &text,
        &["mcp".to_string(), "servers".to_string(), name.to_string()],
        &file,
        &old,
    )?;
    write_config(&file, &text)?;
    say!("{}: added [[mcp.servers]] {name}", file.display());
    Ok(0)
}

fn remove(startup: &Startup, name: &str, project: bool) -> Result<i32, Fail> {
    let file = target_file(startup, project);
    if !file.exists() {
        return Err(not_configured(name, &file));
    }
    let old = read_config(&file);
    let mut doc: toml_edit::DocumentMut = old.parse().map_err(|e: toml_edit::TomlError| {
        toml_parse_fail(
            &file,
            e.to_string()
                .lines()
                .next()
                .unwrap_or("invalid TOML")
                .trim(),
        )
    })?;
    let servers = doc
        .as_table_mut()
        .get_mut("mcp")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|t| t.get_mut("servers"))
        .and_then(toml_edit::Item::as_array_of_tables_mut);
    let Some(servers) = servers else {
        return Err(not_configured(name, &file));
    };
    let Some(index) = servers.iter().position(|t| item_name(t) == Some(name)) else {
        return Err(not_configured(name, &file));
    };
    servers.remove(index);
    write_config(&file, &doc.to_string())?;
    say!("{}: removed [[mcp.servers]] {name}", file.display());
    Ok(0)
}

fn dup_name(name: &str) -> Fail {
    Fail::new(
        codes::CFG_DUPNAME,
        cairn_core::error::ExitStatus::Usage,
        format!("MCP server `{name}` is already configured"),
        Some("names must be unique; `cairn mcp remove <name>` first".to_string()),
    )
}

fn not_configured(name: &str, file: &Path) -> Fail {
    Fail::new(
        codes::CFG_UNKNOWN,
        cairn_core::error::ExitStatus::Usage,
        format!(
            "MCP server `{name}` is not configured in {}",
            file.display()
        ),
        Some("run `cairn mcp list` to see configured servers".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use std::collections::BTreeMap;

    fn startup_in(dir: &std::path::Path) -> Startup {
        let mut loaded = cairn_config::load(&LoadOptions {
            cwd: dir.to_path_buf(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        loaded.paths = Paths {
            config_home: dir.join("config"),
            data_home: dir.join("data"),
            state_home: dir.join("state"),
            cache_home: dir.join("cache"),
        };
        Startup {
            loaded,
            quiet: true,
        }
    }

    #[test]
    fn add_then_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let file = startup.loaded.paths.user_config_file();
        add(
            &startup,
            "docs",
            "stdio",
            "npx",
            &["-y".to_string(), "mcp-docs".to_string()],
            "",
            "",
            false,
        )
        .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("[[mcp.servers]]"), "{text}");
        assert!(text.contains("mcp-docs"), "{text}");
        // Duplicate rejected before any write.
        let err = add(&startup, "docs", "stdio", "npx", &[], "", "", false).unwrap_err();
        assert_eq!(err.code, codes::CFG_DUPNAME);
        // Remove clears it.
        remove(&startup, "docs", false).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("[[mcp.servers]]"), "{text}");
    }

    #[test]
    fn transport_rules_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let err = add(&startup, "x", "carrier-pigeon", "", &[], "", "", false).unwrap_err();
        assert_eq!(err.exit, 2);
        let err = add(&startup, "x", "stdio", "", &[], "", "", false).unwrap_err();
        assert!(err.message.contains("--command"), "{}", err.message);
        let err = add(&startup, "x", "http", "", &[], "", "", false).unwrap_err();
        assert!(err.message.contains("--url"), "{}", err.message);
        let err = remove(&startup, "ghost", false).unwrap_err();
        assert!(err.message.contains("ghost"), "{}", err.message);
    }

    #[test]
    fn project_config_is_used_with_the_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        add(&startup, "p1", "stdio", "cmd", &[], "", "", true).unwrap();
        let project = target_file(&startup, true);
        assert!(project.exists(), "{}", project.display());
        assert!(std::fs::read_to_string(&project)
            .unwrap()
            .contains("[[mcp.servers]]"));
    }
}
