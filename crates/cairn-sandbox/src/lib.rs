//! `cairn-sandbox` — OS sandbox backends for `bash` (SPEC §9.5).
//!
//! M2 carries only the contract the tool layer needs: how strong the
//! confinement is, as §9.5's `Enforced | Advisory | Disabled`, and a way to
//! say so. The Landlock, Seatbelt and restricted-token backends — the only
//! code in the workspace allowed to be `unsafe` — arrive with M4. Until then
//! every tool runs under [`PathChecksOnly`]: the §9.4 boundary is enforced in
//! the tool pipeline, and nothing at the OS level is claimed.

pub mod process;

/// How strongly a platform confines child processes (§9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxLevel {
    /// The OS enforces path and syscall limits on children.
    Enforced,
    /// Only the checks Cairn itself performs apply.
    Advisory,
    /// Nothing is confined (§9.5's Tier-3 row).
    Disabled,
}

impl SandboxLevel {
    /// The word `cairn doctor` prints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "Enforced",
            Self::Advisory => "Advisory",
            Self::Disabled => "Disabled",
        }
    }
}

/// What the tool layer asks of a sandbox.
pub trait Sandbox: Send + Sync {
    /// The mechanism (`landlock+seccomp`, `seatbelt`, `path-checks`, ...).
    fn name(&self) -> &'static str;
    /// How strongly it confines.
    fn level(&self) -> SandboxLevel;
}

/// The M2 sandbox: no OS confinement, honestly labelled `Advisory`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PathChecksOnly;

impl Sandbox for PathChecksOnly {
    fn name(&self) -> &'static str {
        "path-checks"
    }

    fn level(&self) -> SandboxLevel {
        SandboxLevel::Advisory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_m2_sandbox_claims_only_what_it_does() {
        let sandbox = PathChecksOnly;
        assert_eq!(sandbox.level(), SandboxLevel::Advisory);
        assert_eq!(sandbox.level().as_str(), "Advisory");
        assert_eq!(sandbox.name(), "path-checks");
    }
}
