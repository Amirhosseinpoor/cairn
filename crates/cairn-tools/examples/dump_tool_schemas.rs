//! Writes `schemas/tools/<tool>.input.schema.json` and
//! `<tool>.output.schema.json` from the live registry (SPEC §6, REQ-ARCH-009).
//!
//! ```sh
//! cargo run -p cairn-tools --example dump_tool_schemas -- schemas/tools
//! ```

use serde_json::{json, Value};

fn titled(mut schema: Value, title: &str, description: &str) -> String {
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "$schema".into(),
            json!("https://json-schema.org/draft/2020-12/schema"),
        );
        object.insert("title".into(), json!(title));
        object.insert("description".into(), json!(description));
    }
    let mut text = serde_json::to_string_pretty(&schema).expect("schema serialises");
    text.push('\n');
    text
}

fn main() {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "schemas/tools".into()),
    );
    std::fs::create_dir_all(&dir).expect("create output dir");
    let mut registry = cairn_tools::Registry::new();
    cairn_tools::builtin::register_all(&mut registry).expect("tools register");
    let mut written = 0;
    for def in registry.definitions(cairn_core::Mode::Build) {
        let input = titled(
            def.input_schema,
            &format!("{} input", def.name),
            &def.description,
        );
        std::fs::write(dir.join(format!("{}.input.schema.json", def.name)), input).expect("write");
        written += 1;
    }
    for (name, schema) in registry.output_schemas() {
        let text = titled(
            schema,
            &format!("{name} output"),
            "The `data` object of a successful result.",
        );
        std::fs::write(dir.join(format!("{name}.output.schema.json")), text).expect("write");
        written += 1;
    }
    println!("wrote {written} schema(s) to {}", dir.display());
}
