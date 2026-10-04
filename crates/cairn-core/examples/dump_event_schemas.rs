//! Writes `schemas/events/*.schema.json` from the Rust types (REQ-ARCH-009).
//!
//! ```sh
//! cargo run -p cairn-core --example dump_event_schemas -- schemas/events
//! ```

fn main() {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "schemas/events".into()),
    );
    std::fs::create_dir_all(&dir).expect("create output dir");
    let mut written = 0;
    for (name, body) in cairn_core::event::event_schemas() {
        std::fs::write(dir.join(name), body).expect("write schema");
        written += 1;
    }
    println!("wrote {written} schema(s) to {}", dir.display());
}
