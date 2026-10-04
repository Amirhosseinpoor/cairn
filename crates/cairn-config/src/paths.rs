//! Per-OS path resolution (SPEC §11.6, REQ-CLI-007/008).

use std::path::{Path, PathBuf};

/// Function shape used to read the environment (injectable for tests).
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Resolved Cairn directories (SPEC §11.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `<config home>` — holds `config.toml`, `AGENTS.md`, `themes/`, `keybindings.toml`.
    pub config_home: PathBuf,
    /// `<data home>` — holds `sessions/`, `plans/`.
    pub data_home: PathBuf,
    /// `<state home>` — holds `logs/` on XDG; on macOS/Windows it *is* the
    /// logs directory §11.6 names (see [`Paths::default_log_file`]).
    pub state_home: PathBuf,
    /// `<cache home>` — holds `index/`, `trace/`, `compiled/`.
    pub cache_home: PathBuf,
}

fn home_dir(get: EnvLookup<'_>) -> PathBuf {
    #[cfg(windows)]
    {
        get("USERPROFILE").map_or_else(|| PathBuf::from("."), PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        get("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
    }
}

/// Only the Linux/BSD arm of [`Paths::resolve`] reads XDG variables (SPEC
/// §11.6): macOS has `~/Library` and Windows has `%APPDATA%`, so without this
/// gate the function is dead code — and `cargo build` says so — on the two
/// platforms where §11.6 does not use it.
#[cfg(not(any(target_os = "macos", windows)))]
fn xdg_dir(get: EnvLookup<'_>, var: &str, fallback: &str) -> PathBuf {
    match get(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir(get).join(fallback),
    }
}

impl Paths {
    /// Resolve all directories for the current environment.
    ///
    /// `CAIRN_HOME` replaces config/data/cache with `<CAIRN_HOME>/{config,data,cache}`
    /// (REQ-CLI-007); `CAIRN_CACHE_DIR` / `CAIRN_DATA_DIR` override individually
    /// (SPEC §11.3) and are applied by the loader as config sources.
    #[must_use]
    pub fn resolve(get: EnvLookup<'_>) -> Self {
        let (config_home, data_home, state_home, cache_home) = match get("CAIRN_HOME") {
            Some(root) if !root.is_empty() => {
                let root = PathBuf::from(root);
                let data = root.join("data");
                // SPEC §11.6 §REQ-CLI-007: config+data+cache under CAIRN_HOME;
                // logs (state) live under data so tests never touch the real XDG state dir.
                (root.join("config"), data.clone(), data, root.join("cache"))
            }
            _ => {
                #[cfg(target_os = "macos")]
                {
                    // SPEC §11.6's macOS column: config and data under
                    // `~/Library/Application Support`, logs under
                    // `~/Library/Logs`, cache under `~/Library/Caches`.
                    let home = home_dir(get);
                    let support = home.join("Library/Application Support");
                    (
                        support.join("cairn"),
                        support.join("cairn/data"),
                        home.join("Library/Logs/cairn"),
                        home.join("Library/Caches/cairn"),
                    )
                }
                #[cfg(windows)]
                {
                    let home = home_dir(get);
                    let appdata =
                        get("APPDATA").map_or_else(|| home.join("AppData/Roaming"), PathBuf::from);
                    let local = get("LOCALAPPDATA")
                        .map_or_else(|| home.join("AppData/Local"), PathBuf::from);
                    (
                        appdata.join("cairn"),
                        local.join("cairn/data"),
                        local.join("cairn/logs"),
                        local.join("cairn/cache"),
                    )
                }
                #[cfg(not(any(target_os = "macos", windows)))]
                {
                    (
                        xdg_dir(get, "XDG_CONFIG_HOME", ".config").join("cairn"),
                        xdg_dir(get, "XDG_DATA_HOME", ".local/share").join("cairn"),
                        xdg_dir(get, "XDG_STATE_HOME", ".local/state").join("cairn"),
                        xdg_dir(get, "XDG_CACHE_HOME", ".cache").join("cairn"),
                    )
                }
            }
        };

        Self {
            config_home,
            data_home,
            state_home,
            cache_home,
        }
    }

    /// User config file (SPEC §11.6 row 1).
    #[must_use]
    pub fn user_config_file(&self) -> PathBuf {
        self.config_home.join("config.toml")
    }

    /// System config file (SPEC §11.6 last row).
    #[must_use]
    pub fn system_config_file() -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".into()))
                .join("cairn/config.toml")
        }
        #[cfg(not(windows))]
        {
            PathBuf::from("/etc/cairn/config.toml")
        }
    }

    /// Sessions directory (SPEC §11.7).
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_home.join("sessions")
    }

    /// Log file default (SPEC §12.1).
    ///
    /// XDG nests it as `<state home>/logs/cairn.log`, but §11.6's macOS and
    /// Windows rows name a logs *directory* that already carries the `cairn`
    /// segment (`~/Library/Logs/cairn/`, `%LOCALAPPDATA%\cairn\logs\`), so on
    /// those two `state_home` is that directory and nothing is appended.
    #[must_use]
    pub fn default_log_file(&self) -> PathBuf {
        #[cfg(any(target_os = "macos", windows))]
        {
            self.state_home.join("cairn.log")
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            self.state_home.join("logs/cairn.log")
        }
    }

    /// Trace directory default (SPEC §12.1).
    #[must_use]
    pub fn default_trace_dir(&self) -> PathBuf {
        self.cache_home.join("trace")
    }

    /// Index database default (SPEC §5.3).
    #[must_use]
    pub fn default_index_db(&self) -> PathBuf {
        self.cache_home.join("index/index.db")
    }
}

/// Workspace root discovery: explicit override → `CAIRN_WORKSPACE` → nearest
/// `.git` walking up from `cwd` → `cwd` (SPEC §1.6, REQ-CLI-004 context).
#[must_use]
pub fn discover_workspace(cwd: &Path, explicit: Option<&Path>, get: EnvLookup<'_>) -> PathBuf {
    if let Some(ws) = explicit {
        return ws.to_path_buf();
    }
    if let Some(ws) = get("CAIRN_WORKSPACE") {
        if !ws.is_empty() {
            return PathBuf::from(ws);
        }
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

/// `~` expansion for path-valued config fields (SPEC §11.4.2 `E-CFG-BADPATH`).
#[must_use]
pub fn expand_tilde(value: &str, get: EnvLookup<'_>) -> PathBuf {
    if value == "~" {
        return home_dir(get);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir(get).join(rest);
    }
    PathBuf::from(value)
}

/// Path fields MUST be empty, absolute, or `~`-prefixed (SPEC §11.4.2).
#[must_use]
pub fn is_abs_or_tilde(value: &str) -> bool {
    value.is_empty() || value == "~" || value.starts_with("~/") || Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        // §11.6: Windows reads `USERPROFILE` where POSIX reads `HOME`. A
        // fixture that set only `HOME` would resolve `home_dir` to "." there,
        // and every `~` assertion below would be about the wrong directory.
        if let Some(home) = env.get("HOME").cloned() {
            env.entry("USERPROFILE".to_string()).or_insert(home);
        }
        env
    }

    /// Path equality that ignores the separator. §11.6's table is written
    /// with `/` while `Path::join` appends `\` on Windows, and `PathBuf`'s
    /// `PartialEq` compares the raw string — so the two spellings of one
    /// directory compare unequal. REQ-CLI-008 normalises at the OS boundary;
    /// this is the test-side equivalent. `Path::components` treats both bytes
    /// as separators on Windows and neither as special on POSIX, so it needs
    /// no allocation and no platform `cfg`.
    fn same_path(a: &Path, b: &Path) -> bool {
        a.components().eq(b.components())
    }

    #[test]
    fn ca_irn_home_replaces_all_roots() {
        let e = env_of(&[("CAIRN_HOME", "/tmp/cairn-home"), ("HOME", "/home/u")]);
        let get = |k: &str| e.get(k).cloned();
        let p = Paths::resolve(&get);
        assert!(same_path(
            &p.config_home,
            Path::new("/tmp/cairn-home/config")
        ));
        assert!(same_path(&p.data_home, Path::new("/tmp/cairn-home/data")));
        assert!(same_path(&p.cache_home, Path::new("/tmp/cairn-home/cache")));
        assert!(same_path(
            &p.user_config_file(),
            Path::new("/tmp/cairn-home/config/config.toml")
        ));
        assert!(same_path(
            &p.sessions_dir(),
            Path::new("/tmp/cairn-home/data/sessions")
        ));
    }

    /// §11.6's Linux column, plus the `XDG_*` variables only that column
    /// reads. macOS and Windows have their own rows, asserted below.
    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn xdg_defaults_on_linux() {
        let e = env_of(&[("HOME", "/home/u")]);
        let get = |k: &str| e.get(k).cloned();
        let p = Paths::resolve(&get);
        assert!(same_path(
            &p.config_home,
            Path::new("/home/u/.config/cairn")
        ));
        assert!(same_path(
            &p.data_home,
            Path::new("/home/u/.local/share/cairn")
        ));
        assert!(same_path(
            &p.state_home,
            Path::new("/home/u/.local/state/cairn")
        ));
        assert!(same_path(&p.cache_home, Path::new("/home/u/.cache/cairn")));
        assert!(same_path(
            &p.default_log_file(),
            Path::new("/home/u/.local/state/cairn/logs/cairn.log")
        ));
        assert!(same_path(
            &p.default_trace_dir(),
            Path::new("/home/u/.cache/cairn/trace")
        ));
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn xdg_env_overrides() {
        let e = env_of(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/custom/cfg"),
            ("XDG_CACHE_HOME", "/custom/cache"),
        ]);
        let get = |k: &str| e.get(k).cloned();
        let p = Paths::resolve(&get);
        assert!(same_path(&p.config_home, Path::new("/custom/cfg/cairn")));
        assert!(same_path(&p.cache_home, Path::new("/custom/cache/cairn")));
        assert!(same_path(
            &p.data_home,
            Path::new("/home/u/.local/share/cairn")
        ));
    }

    /// §11.6's macOS column.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_defaults_match_section_11_6() {
        let e = env_of(&[("HOME", "/Users/u")]);
        let get = |k: &str| e.get(k).cloned();
        let p = Paths::resolve(&get);
        assert!(same_path(
            &p.config_home,
            Path::new("/Users/u/Library/Application Support/cairn")
        ));
        assert!(same_path(
            &p.data_home,
            Path::new("/Users/u/Library/Application Support/cairn/data")
        ));
        assert!(same_path(
            &p.state_home,
            Path::new("/Users/u/Library/Logs/cairn")
        ));
        assert!(same_path(
            &p.cache_home,
            Path::new("/Users/u/Library/Caches/cairn")
        ));
        assert!(same_path(
            &p.default_log_file(),
            Path::new("/Users/u/Library/Logs/cairn/cairn.log")
        ));
        assert!(same_path(
            &p.default_trace_dir(),
            Path::new("/Users/u/Library/Caches/cairn/trace")
        ));
    }

    /// §11.6's Windows column.
    #[cfg(windows)]
    #[test]
    fn windows_defaults_match_section_11_6() {
        let e = env_of(&[
            ("HOME", r"C:\Users\u"),
            ("APPDATA", r"C:\Users\u\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\u\AppData\Local"),
        ]);
        let get = |k: &str| e.get(k).cloned();
        let p = Paths::resolve(&get);
        let roaming = PathBuf::from(r"C:\Users\u\AppData\Roaming");
        let local = PathBuf::from(r"C:\Users\u\AppData\Local");
        assert!(same_path(&p.config_home, &roaming.join("cairn")));
        assert!(same_path(&p.data_home, &local.join("cairn").join("data")));
        assert!(same_path(&p.state_home, &local.join("cairn").join("logs")));
        assert!(same_path(&p.cache_home, &local.join("cairn").join("cache")));
        assert!(same_path(
            &p.default_log_file(),
            &p.state_home.join("cairn.log")
        ));
        assert!(same_path(
            &p.default_trace_dir(),
            &p.cache_home.join("trace")
        ));
    }

    #[test]
    fn workspace_discovery_prefers_git_root() {
        let tmp = std::env::temp_dir().join(format!("cairn-ws-{}", std::process::id()));
        let nested = tmp.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        let e = env_of(&[]);
        let get = |k: &str| e.get(k).cloned();
        assert!(same_path(&discover_workspace(&nested, None, &get), &tmp));

        // explicit beats everything
        assert!(same_path(
            &discover_workspace(&nested, Some(Path::new("/explicit")), &get),
            Path::new("/explicit")
        ));
        // env beats git discovery
        let e2 = env_of(&[("CAIRN_WORKSPACE", "/from-env")]);
        let get2 = |k: &str| e2.get(k).cloned();
        assert!(same_path(
            &discover_workspace(&nested, None, &get2),
            Path::new("/from-env")
        ));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn path_validation_rules() {
        assert!(is_abs_or_tilde(""));
        assert!(is_abs_or_tilde("~/logs/x.log"));
        assert!(is_abs_or_tilde("~"));
        // §11.4.2 says "absolute", and that word is platform-relative: a
        // Windows path needs a drive prefix, so `/var/log/x` is absolute on
        // the POSIX arm only.
        #[cfg(unix)]
        assert!(is_abs_or_tilde("/var/log/x"));
        #[cfg(windows)]
        assert!(is_abs_or_tilde(r"C:\var\log\x"));
        assert!(!is_abs_or_tilde("relative/path"));
        assert!(!is_abs_or_tilde("x.log"));
    }

    #[test]
    fn tilde_expansion() {
        let e = env_of(&[("HOME", "/home/u")]);
        let get = |k: &str| e.get(k).cloned();
        assert!(same_path(
            &expand_tilde("~/a/b", &get),
            Path::new("/home/u/a/b")
        ));
        assert!(same_path(&expand_tilde("~", &get), Path::new("/home/u")));
        assert!(same_path(&expand_tilde("/abs", &get), Path::new("/abs")));
    }
}
