//! Validation issues and config-layer provenance.

use std::path::PathBuf;

/// Winning layer for an effective config key (SPEC §11.5 REQ-CLI-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Built-in default (SPEC §11.4.1).
    Default,
    /// `/etc/cairn/config.toml` (or OS equivalent).
    System,
    /// `$XDG_CONFIG_HOME/cairn/config.toml`.
    User,
    /// `~/.config/cairn/profiles/<name>.toml`.
    Profile(String),
    /// `.cairn/config.toml` in the workspace or a parent directory.
    Project,
    /// `--config FILE`.
    Explicit,
    /// `CAIRN_*` environment variable (name recorded).
    Env(String),
    /// CLI flag (flag name recorded).
    Flag(String),
}

impl Source {
    /// Stable layer name used in `--effective` output.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::System => "system",
            Self::User => "user",
            Self::Profile(_) => "profile",
            Self::Project => "project",
            Self::Explicit => "explicit",
            Self::Env(_) => "env",
            Self::Flag(_) => "flag",
        }
    }

    /// Detail suffix (`env:CAIRN_MODEL`, `flag:-m`, …).
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Profile(p) => Some(p.as_str()),
            Self::Env(v) | Self::Flag(v) => Some(v.as_str()),
            _ => None,
        }
    }
}

/// One validation problem. All problems are collected before returning
/// (REQ-CLI-003: never fail fast).
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    /// Stable code (`E-CFG-*`, `W-CFG-*`).
    pub code: &'static str,
    pub message: String,
    /// Dotted config key, when the issue is key-scoped.
    pub path: Option<String>,
    /// File the key came from, when known.
    pub file: Option<PathBuf>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// `false` → surfaced as `W-CFG-PARTIAL` at startup for sections the running
    /// command does not need (REQ-CLI-004).
    pub fatal: bool,
    /// Config section name (`providers`, `ui`, …, or `""` for root keys).
    pub section: &'static str,
}

impl Issue {
    #[must_use]
    pub fn error(
        code: &'static str,
        message: impl Into<String>,
        path: Option<&str>,
        section: &'static str,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            path: path.map(str::to_string),
            file: None,
            line: None,
            column: None,
            fatal: fatal_section(section),
            section,
        }
    }

    #[must_use]
    pub fn warn(
        code: &'static str,
        message: impl Into<String>,
        path: Option<&str>,
        section: &'static str,
    ) -> Self {
        let mut i = Self::error(code, message, path, section);
        i.fatal = false;
        i
    }

    #[must_use]
    pub fn at(mut self, file: impl Into<PathBuf>, line: usize, column: usize) -> Self {
        self.file = Some(file.into());
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    #[must_use]
    pub fn in_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// `path:line:column` when available (REQ-CLI-003).
    #[must_use]
    pub fn location(&self) -> Option<String> {
        match (&self.file, self.line) {
            (Some(f), Some(l)) => Some(match self.column {
                Some(c) => format!("{}:{}:{}", f.display(), l, c),
                None => format!("{}:{}", f.display(), l),
            }),
            (Some(f), None) => Some(format!("{}", f.display())),
            _ => None,
        }
    }

    /// Single line for CLI output: `<location>: <code>: <message>` or `<code>: <message>`.
    #[must_use]
    pub fn render(&self) -> String {
        match self.location() {
            Some(loc) => format!("{loc}: {}: {}", self.code, self.message),
            None => format!("{}: {}", self.code, self.message),
        }
    }
}

/// Sections whose errors abort startup with exit 2 (REQ-CLI-004).
#[must_use]
pub fn fatal_section(section: &str) -> bool {
    matches!(section, "" | "providers" | "model" | "security")
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_match_spec() {
        assert_eq!(Source::Default.as_str(), "default");
        assert_eq!(Source::System.as_str(), "system");
        assert_eq!(Source::User.as_str(), "user");
        assert_eq!(Source::Project.as_str(), "project");
        assert_eq!(Source::Flag("-m".into()).as_str(), "flag");
        assert_eq!(Source::Env("CAIRN_MODEL".into()).as_str(), "env");
        assert_eq!(
            Source::Env("CAIRN_MODEL".into()).detail(),
            Some("CAIRN_MODEL")
        );
        assert_eq!(Source::Profile("work".into()).detail(), Some("work"));
    }

    #[test]
    fn fatal_sections_match_req_cli_004() {
        assert!(fatal_section(""));
        assert!(fatal_section("providers"));
        assert!(fatal_section("model"));
        assert!(fatal_section("security"));
        assert!(!fatal_section("mcp"));
        assert!(!fatal_section("ui"));
        assert!(!fatal_section("verify"));
    }

    #[test]
    fn issue_render_includes_location_and_code() {
        let i = Issue::error(
            "E-CFG-RANGE",
            "temperature = 5 is outside 0..=2",
            Some("temperature"),
            "",
        )
        .at("/etc/cairn/config.toml", 4, 12);
        assert_eq!(
            i.render(),
            "/etc/cairn/config.toml:4:12: E-CFG-RANGE: temperature = 5 is outside 0..=2"
        );
        let j = Issue::error(
            "E-CFG-SUM",
            "weights sum to 0.9",
            Some("repo_map.weights"),
            "repo_map",
        );
        assert_eq!(j.render(), "E-CFG-SUM: weights sum to 0.9");
        assert!(j.location().is_none());
    }
}
