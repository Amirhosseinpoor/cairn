//! `cairn version` (SPEC §11.1, REQ-PROD-001, T-XPLAT-001).

use crate::args::{Cli, VersionArgs};
use crate::output::Fail;
use cairn_config::{registry_updated_at, SCHEMA_VERSION};
use serde_json::json;

/// Support tier of a target triple (SPEC §1.5 platform matrix).
pub(crate) const fn tier(target: &str) -> u8 {
    // Tier 1 — Linux x86_64 (glibc + musl), macOS arm64/x86_64 (and WSL: Linux).
    const TIER1: &[&str] = &[
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ];
    // Tier 2 — Windows x86_64, Linux aarch64.
    const TIER2: &[&str] = &[
        "x86_64-pc-windows-msvc",
        "x86_64-pc-windows-gnu",
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
    ];
    let mut i = 0;
    while i < TIER1.len() {
        if bytes_eq(target.as_bytes(), TIER1[i].as_bytes()) {
            return 1;
        }
        i += 1;
    }
    i = 0;
    while i < TIER2.len() {
        if bytes_eq(target.as_bytes(), TIER2[i].as_bytes()) {
            return 2;
        }
        i += 1;
    }
    3
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Build profile the binary was compiled with.
const fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

pub fn run(cli: &Cli, args: &VersionArgs) -> Result<i32, Fail> {
    let target = env!("CAIRN_TARGET");
    if args.json || cli.wants_json() {
        // Fixed key set — identical JSON schema on every Tier-1 target.
        let v = json!({
            "name": "cairn",
            "version": env!("CARGO_PKG_VERSION"),
            "target": target,
            "profile": profile(),
            "rustc": env!("CAIRN_RUSTC"),
            "tier": tier(target),
            "config_schema_version": SCHEMA_VERSION,
            "session_schema_version": 1,
            "registry_updated": registry_updated_at(),
        });
        say!("{}", serde_json::to_string_pretty(&v).expect("static json"));
    } else {
        say!(
            "cairn {} ({} {}, {}, rustc {})",
            env!("CARGO_PKG_VERSION"),
            target,
            profile(),
            format_args!("tier {}", tier(target)),
            env!("CAIRN_RUSTC"),
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_match_the_platform_matrix() {
        assert_eq!(tier("x86_64-unknown-linux-gnu"), 1);
        assert_eq!(tier("x86_64-unknown-linux-musl"), 1);
        assert_eq!(tier("aarch64-apple-darwin"), 1);
        assert_eq!(tier("x86_64-pc-windows-msvc"), 2);
        assert_eq!(tier("aarch64-unknown-linux-gnu"), 2);
        assert_eq!(tier("x86_64-unknown-freebsd"), 3);
    }

    #[test]
    fn version_json_is_stable_shape() {
        let v = json!({
            "name": "cairn",
            "version": env!("CARGO_PKG_VERSION"),
            "target": env!("CAIRN_TARGET"),
            "profile": profile(),
            "rustc": env!("CAIRN_RUSTC"),
            "tier": tier(env!("CAIRN_TARGET")),
            "config_schema_version": SCHEMA_VERSION,
            "session_schema_version": 1,
            "registry_updated": registry_updated_at(),
        });
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "name",
                "version",
                "target",
                "profile",
                "rustc",
                "tier",
                "config_schema_version",
                "session_schema_version",
                "registry_updated",
            ]
        );
        assert_eq!(v["config_schema_version"], 1);
        assert_ne!(registry_updated_at(), "");
    }
}
