//! Operating modes (SPEC §7).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// The four operating modes (SPEC §7.1). Serialized as `plan`, `build`, `auto`,
/// `auto-unsafe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum Mode {
    /// Read-only analysis (SPEC §7.1).
    Plan,
    /// Execution with per-action approval (SPEC §7.1).
    #[default]
    Build,
    /// Autonomous execution within guardrails (SPEC §7.1).
    Auto,
    /// Full bypass; requires `--dangerously-skip-permissions` (SPEC §7.1, G-M1).
    AutoUnsafe,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Plan, Mode::Build, Mode::Auto, Mode::AutoUnsafe];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Auto => "auto",
            Self::AutoUnsafe => "auto-unsafe",
        }
    }

    /// Short status-bar pill label (SPEC §10.1).
    #[must_use]
    pub const fn pill(self) -> &'static str {
        match self {
            Self::Plan => "[Plan]",
            Self::Build => "[Build]",
            Self::Auto => "[Auto]",
            Self::AutoUnsafe => "[!Unstable]",
        }
    }

    /// Whether the mode permits write/execute side effects at all (SPEC §7.2).
    #[must_use]
    pub const fn allows_side_effects(self) -> bool {
        !matches!(self, Self::Plan)
    }

    /// Whether the mode requires a per-action approval (SPEC §7.2).
    #[must_use]
    pub const fn asks_everything(self) -> bool {
        matches!(self, Self::Build)
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        s.parse().map_err(|_| {
            format!("invalid mode {s:?}; valid values: plan, build, auto, auto-unsafe")
        })
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "plan" => Ok(Self::Plan),
            "build" => Ok(Self::Build),
            "auto" => Ok(Self::Auto),
            "auto-unsafe" | "auto_unsafe" => Ok(Self::AutoUnsafe),
            other => Err(format!(
                "invalid mode {other:?}; valid values: plan, build, auto, auto-unsafe"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_serde_uses_spec_strings() {
        for m in Mode::ALL {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json.trim_matches('"'), m.as_str());
            let back: Mode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
        assert_eq!(
            serde_json::to_string(&Mode::AutoUnsafe).unwrap(),
            r#""auto-unsafe""#
        );
    }

    #[test]
    fn mode_default_is_build() {
        assert_eq!(Mode::default(), Mode::Build);
    }

    #[test]
    fn mode_flags_match_spec_matrix() {
        assert!(!Mode::Plan.allows_side_effects());
        assert!(Mode::Build.allows_side_effects());
        assert!(Mode::Auto.allows_side_effects());
        assert!(Mode::AutoUnsafe.allows_side_effects());
        assert!(Mode::Build.asks_everything());
        assert!(!Mode::Auto.asks_everything());
        assert_eq!(Mode::AutoUnsafe.pill(), "[!Unstable]");
    }

    #[test]
    fn mode_from_str_accepts_and_rejects() {
        assert_eq!("auto-unsafe".parse::<Mode>().unwrap(), Mode::AutoUnsafe);
        assert_eq!("auto_unsafe".parse::<Mode>().unwrap(), Mode::AutoUnsafe);
        assert!("unsafe".parse::<Mode>().is_err());
        assert!(Mode::parse("nope")
            .unwrap_err()
            .contains("plan, build, auto"));
    }
}
