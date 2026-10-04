//! Embeds build metadata used by `cairn version` (SPEC §11.1, T-XPLAT-001).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=CAIRN_TARGET={target}");
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    if let Ok(out) = std::process::Command::new(&rustc).arg("--version").output() {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            println!("cargo:rustc-env=CAIRN_RUSTC={}", text.trim());
        }
    }
}
