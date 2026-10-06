//! The tool registry (SPEC §6.1, REQ-TOOL-001/002).

use std::collections::BTreeMap;
use std::sync::Arc;

use cairn_core::Mode;
use serde_json::Value;

use crate::schema::Compiled;
use crate::tool::Tool;
use crate::types::{PermissionClass, SideEffect};

/// A tool as the model is shown it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One row of §6.1's table, from live registration (`cairn doctor --tools`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub name: &'static str,
    pub class: &'static str,
    pub side_effect: &'static str,
    pub idempotency: &'static str,
    pub timeout_ms: u64,
    pub max_output_bytes: u32,
    pub serial: bool,
}

/// A tool could not be registered.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("tool `{0}` is already registered")]
    Duplicate(&'static str),
    #[error("tool name `{0}` must match ^[a-z][a-z0-9_]{{1,63}}$")]
    BadName(&'static str),
    #[error("tool `{name}`: {reason}")]
    Invalid { name: &'static str, reason: String },
}

struct Entry {
    tool: Arc<dyn Tool>,
    input: Compiled,
}

/// All registered tools, by name.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<&'static str, Entry>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.entries.keys()).finish()
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() >= 2
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// REQ-MODE-001 / REQ-TOOL-002: a tool whose side effect writes or executes
/// is not offered in `plan` — it is absent from the request, not merely
/// refused. `todo_write` (class `WriteState`) is the one write allowed in
/// every mode (§6.1).
#[must_use]
pub fn offered_in(mode: Mode, tool: &dyn Tool) -> bool {
    if mode != Mode::Plan {
        return true;
    }
    if tool.permission_class() == PermissionClass::WriteState {
        return true;
    }
    !matches!(tool.side_effect(), SideEffect::Write | SideEffect::Execute)
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tool`, checking everything §6.1 and §6.5 promise about it.
    ///
    /// # Errors
    /// [`RegistryError`] for a duplicate or malformed name, an invalid or
    /// open (`additionalProperties` not `false`) input schema, an overlong
    /// description, or a zero output limit.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegistryError> {
        let name = tool.name();
        if !valid_name(name) {
            return Err(RegistryError::BadName(name));
        }
        if self.entries.contains_key(name) {
            return Err(RegistryError::Duplicate(name));
        }
        let invalid = |reason: String| RegistryError::Invalid { name, reason };
        if tool.description().chars().count() > 2000 {
            return Err(invalid("description is over 2,000 characters".to_string()));
        }
        if tool.max_output_bytes() == 0 {
            return Err(invalid("max_output_bytes is zero".to_string()));
        }
        let schema = tool.input_schema();
        if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(invalid(
                "the input schema must set additionalProperties: false".to_string(),
            ));
        }
        let input = Compiled::new(&schema).map_err(|e| invalid(format!("input schema: {e}")))?;
        Compiled::new(&tool.output_schema()).map_err(|e| invalid(format!("output schema: {e}")))?;
        self.entries.insert(name, Entry { tool, input });
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.entries.get(name).map(|e| &e.tool)
    }

    pub(crate) fn validator(&self, name: &str) -> Option<&Compiled> {
        self.entries.get(name).map(|e| &e.input)
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.entries.keys().copied().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The definitions to send the model in `mode`.
    #[must_use]
    pub fn definitions(&self, mode: Mode) -> Vec<ToolDef> {
        self.entries
            .values()
            .filter(|e| offered_in(mode, e.tool.as_ref()))
            .map(|e| ToolDef {
                name: e.tool.name().to_string(),
                description: e.tool.description().to_string(),
                input_schema: e.tool.input_schema(),
            })
            .collect()
    }

    /// Each tool's output schema, by name.
    #[must_use]
    pub fn output_schemas(&self) -> Vec<(&'static str, serde_json::Value)> {
        self.entries
            .values()
            .map(|e| (e.tool.name(), e.tool.output_schema()))
            .collect()
    }

    /// §6.1's table.
    #[must_use]
    pub fn table(&self) -> Vec<TableRow> {
        self.entries
            .values()
            .map(|e| TableRow {
                name: e.tool.name(),
                class: e.tool.permission_class().as_str(),
                side_effect: e.tool.side_effect().as_str(),
                idempotency: e.tool.idempotency().as_str(),
                timeout_ms: u64::try_from(e.tool.timeout().as_millis()).unwrap_or(u64::MAX),
                max_output_bytes: e.tool.max_output_bytes(),
                serial: e.tool.requires_serial(),
            })
            .collect()
    }
}
