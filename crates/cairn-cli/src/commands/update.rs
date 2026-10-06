//! `cairn update [--check] [--version X] [--yes]` (SPEC §1.5, §12.4,
//! REQ-OPS-006/007, T-OPS-002).
//!
//! What is built: reading the signed-release manifest, comparing versions,
//! picking this platform's asset, `--check`, and refusing to replace the
//! binary while a turn runs (exit 10).
//!
//! What is not, and why: installing. §1.5 requires the SHA-256 *and* a
//! minisign signature against the bundled `cairn-release.pub` before any
//! binary is replaced, and REQ-OPS-006 requires that key to be embedded at
//! build time. No release key exists yet, so there is nothing to verify
//! against — and an installer that skips verification is worse than none. The
//! install path therefore stops with `E-UPDATE-SIGNATURE` after validating
//! everything it can, and lands with the key at M5.

use std::cmp::Ordering;
use std::time::Duration;

use cairn_core::error::{codes, ExitStatus};
use serde::Deserialize;

use crate::args::UpdateArgs;
use crate::commands::Startup;
use crate::output::Fail;

/// §1.5's manifest location; `CAIRN_UPDATE_URL` replaces it for tests and
/// mirrors.
pub const MANIFEST_URL: &str = "https://releases.cairn.dev/stable.json";

/// One downloadable build.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub target: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub minisig: String,
}

/// `stable.json` (§1.5).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub released_at: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// `major.minor.patch[-pre]`, enough to order releases.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim().trim_start_matches('v');
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (text, None),
    };
    let mut parts = core.split('.');
    let mut numbers = [0_u64; 3];
    for slot in &mut numbers {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Version { core: numbers, pre })
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| {
            // A pre-release sorts before its release (1.2.0-rc1 < 1.2.0).
            match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `target` (a release triple such as `x86_64-unknown-linux-musl`)
/// names the machine running this binary.
fn matches_host(target: &str, arch: &str, os: &str) -> bool {
    let target = target.to_ascii_lowercase();
    let os_words: &[&str] = match os {
        "linux" => &["linux"],
        "macos" => &["darwin", "apple", "macos"],
        "windows" => &["windows"],
        _ => return false,
    };
    target.contains(arch) && os_words.iter().any(|word| target.contains(word))
}

fn host_asset(manifest: &Manifest) -> Option<&Asset> {
    manifest
        .assets
        .iter()
        .find(|asset| matches_host(&asset.target, std::env::consts::ARCH, std::env::consts::OS))
}

fn fail(code: &'static str, message: impl Into<String>, hint: &str) -> Fail {
    Fail::new(code, ExitStatus::Generic, message, Some(hint.to_string()))
}

/// Plain `http` is allowed for loopback only (a local mirror or a test);
/// everything else must be `https`, so a manifest cannot be downgraded.
fn check_url(url: &str) -> Result<(), Fail> {
    let lower = url.to_ascii_lowercase();
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    if lower.starts_with("https://") || loopback {
        Ok(())
    } else {
        Err(fail(
            "ERR_GENERIC",
            format!("update URL {url} is not https"),
            "set CAIRN_UPDATE_URL to an https:// URL",
        ))
    }
}

async fn fetch_manifest(url: &str) -> Result<Manifest, Fail> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| {
            fail(
                "ERR_GENERIC",
                format!("cannot build an HTTP client: {e}"),
                "check your TLS setup",
            )
        })?;
    let request = client.get(url).build().map_err(|e| {
        fail(
            "ERR_GENERIC",
            format!("{url} is not a usable URL: {e}"),
            "check CAIRN_UPDATE_URL",
        )
    })?;
    let response = client.execute(request).await.map_err(|e| {
        fail(
            "ERR_GENERIC",
            format!("cannot reach {url}: {e}"),
            "check your network, or retry later",
        )
    })?;
    if !response.status().is_success() {
        return Err(fail(
            "ERR_GENERIC",
            format!("{url} answered HTTP {}", response.status().as_u16()),
            "retry later",
        ));
    }
    let bytes = response.bytes().await.map_err(|e| {
        fail(
            "ERR_GENERIC",
            format!("reading {url} failed: {e}"),
            "retry later",
        )
    })?;
    parse_manifest(&bytes)
}

fn parse_manifest(bytes: &[u8]) -> Result<Manifest, Fail> {
    let manifest: Manifest = serde_json::from_slice(bytes).map_err(|e| {
        fail(
            "ERR_GENERIC",
            format!("the release manifest is not valid: {e}"),
            "report this if it persists",
        )
    })?;
    if parse_version(&manifest.version).is_none() {
        return Err(fail(
            "ERR_GENERIC",
            format!("the release manifest names version `{}`", manifest.version),
            "report this if it persists",
        ));
    }
    Ok(manifest)
}

/// What `--check` prints, and whether an update is on offer.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    UpToDate,
    Available {
        version: String,
        sha256: Option<String>,
    },
}

fn verdict(current: &str, manifest: &Manifest) -> Result<Verdict, Fail> {
    let have = parse_version(current).ok_or_else(|| {
        fail(
            "ERR_GENERIC",
            format!("this build's version `{current}` is not a version"),
            "reinstall",
        )
    })?;
    let offered = parse_version(&manifest.version).expect("validated by parse_manifest");
    if offered <= have {
        return Ok(Verdict::UpToDate);
    }
    Ok(Verdict::Available {
        version: manifest.version.trim_start_matches('v').to_string(),
        sha256: host_asset(manifest).map(|asset| asset.sha256.clone()),
    })
}

fn render(verdict: &Verdict) -> String {
    match verdict {
        Verdict::UpToDate => "up to date".to_string(),
        Verdict::Available {
            version,
            sha256: Some(sha),
        } => format!("{version} available (sha256 {sha})"),
        Verdict::Available {
            version,
            sha256: None,
        } => format!("{version} available (no build published for this platform)"),
    }
}

/// `cairn update`.
///
/// # Errors
/// Exit 10 while a turn runs; `E-UPDATE-SIGNATURE` when asked to install
/// (no release key yet); exit 1 for an unreachable or malformed manifest.
pub fn run(cli: &crate::args::Cli, args: &UpdateArgs, startup: &Startup) -> Result<i32, Fail> {
    if cli.offline || startup.loaded.config.network.offline {
        return Err(fail(
            "ERR_GENERIC",
            "offline: cannot reach the release server",
            "drop --offline, or `network.offline`",
        ));
    }
    let url = std::env::var("CAIRN_UPDATE_URL").unwrap_or_else(|_| MANIFEST_URL.to_string());
    check_url(&url)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            fail(
                "ERR_GENERIC",
                format!("cannot start the async runtime: {e}"),
                "retry",
            )
        })?;
    let manifest = runtime.block_on(fetch_manifest(&url))?;
    let current = env!("CARGO_PKG_VERSION");

    if args.check {
        let text = render(&verdict(current, &manifest)?);
        crate::output::say(format_args!("{text}"));
        return Ok(0);
    }

    // §1.5: never replace the binary under a running turn.
    if !crate::activity::active(&startup.loaded.paths.cache_home).is_empty() {
        return Err(Fail::new(
            "ERR_BUSY",
            ExitStatus::Busy,
            "cannot update: a turn is in progress",
            Some("let the running turn finish, then retry".to_string()),
        ));
    }
    if let Some(wanted) = &args.version {
        let wanted_v = parse_version(wanted);
        let offered_v = parse_version(&manifest.version);
        if wanted_v.is_none() || wanted_v != offered_v {
            return Err(fail(
                "ERR_GENERIC",
                format!(
                    "version {wanted} is not published (the stable channel is {})",
                    manifest.version
                ),
                "run `cairn update --check` to see what is on offer",
            ));
        }
    }
    match verdict(current, &manifest)? {
        Verdict::UpToDate => {
            crate::output::say(format_args!("up to date"));
            Ok(0)
        }
        Verdict::Available { version, sha256 } => {
            let asset = if sha256.is_some() {
                ""
            } else {
                " (and no build exists for this platform)"
            };
            Err(Fail::new(
                codes::UPDATE_SIGNATURE,
                ExitStatus::Generic,
                format!(
                    "refusing to install {version}{asset}: this build carries no release public key \
                     (`cairn-release.pub`, REQ-OPS-006), so the download cannot be verified"
                ),
                Some("install the release from the GitHub Releases page and verify its minisign signature yourself".to_string()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, targets: &[&str]) -> Manifest {
        Manifest {
            version: version.to_string(),
            released_at: String::new(),
            assets: targets
                .iter()
                .map(|t| Asset {
                    target: (*t).to_string(),
                    url: format!("https://example.invalid/{t}"),
                    sha256: format!("sha-{t}"),
                    minisig: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn versions_order_like_semver() {
        let v = |s: &str| parse_version(s).expect(s);
        assert!(v("1.2.0") > v("1.1.9"));
        assert!(v("1.10.0") > v("1.9.0"), "numeric, not lexical");
        assert!(v("1.2.0") > v("1.2.0-rc1"), "a release beats its candidate");
        assert!(v("1.2.0-rc2") > v("1.2.0-rc1"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        for bad in ["", "1.2", "1.2.3.4", "a.b.c", "1.2.x"] {
            assert!(parse_version(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_host_asset_is_picked_by_arch_and_os() {
        assert!(matches_host("x86_64-unknown-linux-musl", "x86_64", "linux"));
        assert!(!matches_host(
            "aarch64-unknown-linux-musl",
            "x86_64",
            "linux"
        ));
        assert!(!matches_host("x86_64-pc-windows-msvc", "x86_64", "linux"));
        assert!(matches_host("aarch64-apple-darwin", "aarch64", "macos"));
        assert!(matches_host("x86_64-pc-windows-msvc", "x86_64", "windows"));
        assert!(!matches_host("x86_64-unknown-freebsd", "x86_64", "freebsd"));
    }

    /// REQ-OPS-007's two sentences, exactly.
    #[test]
    fn check_prints_up_to_date_or_the_version_and_digest() {
        let host = format!(
            "{}-unknown-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
        let newer = manifest("99.0.0", &[&host]);
        assert_eq!(
            render(&verdict("0.1.0", &newer).expect("verdict")),
            format!("99.0.0 available (sha256 sha-{host})")
        );
        assert_eq!(
            render(&verdict("99.0.0", &newer).expect("verdict")),
            "up to date"
        );
        assert_eq!(
            render(&verdict("100.0.0", &newer).expect("verdict")),
            "up to date",
            "a build newer than stable is not offered a downgrade"
        );
        let elsewhere = manifest("99.0.0", &["riscv64-unknown-plan9"]);
        assert_eq!(
            render(&verdict("0.1.0", &elsewhere).expect("verdict")),
            "99.0.0 available (no build published for this platform)"
        );
    }

    #[test]
    fn a_manifest_without_a_real_version_is_rejected() {
        assert!(parse_manifest(br#"{"version":"soon"}"#).is_err());
        assert!(parse_manifest(b"not json").is_err());
        assert!(parse_manifest(br#"{"version":"1.2.3"}"#).is_ok());
    }

    #[test]
    fn only_https_or_loopback_http_is_accepted() {
        assert!(check_url("https://releases.cairn.dev/stable.json").is_ok());
        assert!(check_url("http://127.0.0.1:8080/stable.json").is_ok());
        assert!(check_url("http://localhost/stable.json").is_ok());
        assert!(check_url("http://releases.cairn.dev/stable.json").is_err());
        assert!(check_url("ftp://x/y").is_err());
    }
}
