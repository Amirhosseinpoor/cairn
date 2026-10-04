//! Writes the annotated JSON Schema (SPEC §11.4.3) for the config file.
//!
//! ```sh
//! cargo run -p cairn-config --example dump_schema                 # in place
//! cargo run -p cairn-config --example dump_schema -- /tmp/out.json # to a path
//! ```
//!
//! `scripts/check-schemas.sh` uses the second form so it can diff the
//! generated document against the committed one without touching the tree.
fn main() {
    let out = std::env::args().nth(1).map_or_else(
        || {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../schemas/config.schema.json")
        },
        std::path::PathBuf::from,
    );
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("create output dir");
    }
    std::fs::write(&out, format!("{}\n", cairn_config::json_schema_pretty()))
        .expect("write schema");
    eprintln!("wrote {}", out.display());
}
