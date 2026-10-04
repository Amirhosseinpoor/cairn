//! Error reporting and shared output helpers (SPEC §10.9, §11.2, REQ-CLI-002).

use cairn_core::error::ExitStatus;

/// Write to stdout, ignoring a closed pipe (`cairn config list | head`).
pub fn say(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_fmt(args);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Write to stderr, ignoring a closed pipe.
pub fn yell(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = err.write_fmt(args);
    let _ = err.write_all(b"\n");
    let _ = err.flush();
}

/// Anything that can become the optional `hint:` line of a failure.
pub trait IntoHint {
    fn into_hint(self) -> Option<String>;
}

impl IntoHint for String {
    fn into_hint(self) -> Option<String> {
        Some(self)
    }
}

impl IntoHint for &str {
    fn into_hint(self) -> Option<String> {
        Some(self.to_string())
    }
}

impl IntoHint for Option<String> {
    fn into_hint(self) -> Option<String> {
        self
    }
}

/// A user-facing failure: stable code, exit status, message and hint.
#[derive(Debug, Clone)]
pub struct Fail {
    pub code: &'static str,
    pub exit: i32,
    pub message: String,
    pub hint: Option<String>,
}

impl Fail {
    #[must_use]
    pub fn new(
        code: &'static str,
        status: ExitStatus,
        message: impl Into<String>,
        hint: impl IntoHint,
    ) -> Self {
        Self {
            code,
            exit: status.code(),
            message: message.into(),
            hint: hint.into_hint(),
        }
    }

    /// Usage error (exit 2, SPEC §11.2 `ERR_USAGE`).
    #[must_use]
    pub fn usage(message: impl Into<String>, hint: impl IntoHint) -> Self {
        Self::new(
            cairn_core::error::codes::CLI_USAGE,
            ExitStatus::Usage,
            message,
            hint,
        )
    }

    /// Not-found error (exit 9).
    #[must_use]
    pub fn not_found(message: impl Into<String>, hint: impl IntoHint) -> Self {
        Self::new(
            cairn_core::error::codes::SESS_NOTFOUND,
            ExitStatus::NotFound,
            message,
            hint,
        )
    }

    /// Interim failure for a command whose milestone has not landed yet.
    #[must_use]
    pub fn not_implemented(feature: &str, milestone: &str) -> Self {
        Self::new(
            cairn_core::error::codes::IMPL_STAGE,
            ExitStatus::Generic,
            format!("{feature} is not implemented yet (delivered in milestone {milestone})"),
            Some("see SPEC.md §15.4 for the milestone plan and PROGRESS.md for status".to_string()),
        )
    }

    /// Render the failure: the code line always, the hint unless `--quiet`
    /// (REQ-CLI-002).
    pub fn report(&self, quiet: bool) {
        eprintln!("error: {}: {}", self.code, self.message);
        if !quiet {
            if let Some(hint) = &self.hint {
                eprintln!("hint: {hint}");
            }
        }
    }
}

/// Non-fatal startup issue (SPEC §11.4.2 `W-CFG-*`, REQ-CLI-004).
pub fn warn_line(code: &str, message: &str) {
    eprintln!("warning: {code}: {message}");
}

/// `cairn doctor` row status (SPEC §12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl Status {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// Truncate a string for single-line diagnostics.
#[must_use]
pub fn ellipsize(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsize_is_char_aware() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("héllo wörld", 6), "héllo…");
        assert_eq!(ellipsize("a\nb", 10), "a b");
    }

    #[test]
    fn fail_rendering_keeps_code_first() {
        let f = Fail::not_found(
            "session 'abc' not found",
            Some("run `cairn sessions`".to_string()),
        );
        assert_eq!(f.code, "E-SESS-NOTFOUND");
        assert_eq!(f.exit, 9);
        assert!(f.message.contains("abc"));
    }

    #[test]
    fn not_implemented_names_the_milestone() {
        let f = Fail::not_implemented("`cairn run`", "M1");
        assert_eq!(f.exit, 1);
        assert!(f.message.contains("M1"), "{}", f.message);
    }

    #[test]
    fn usage_exit_is_two() {
        assert_eq!(Fail::usage("bad", None).exit, 2);
        assert_eq!(Fail::usage("bad", "hint").code, "E-CLI-USAGE");
    }
}
