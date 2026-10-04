//! Stable error codes and process exit statuses (SPEC §4.5, §6.2, §11.2, D-15).
//!
//! Every user-facing and model-facing failure carries a code from the [`codes`]
//! registry. Codes are compile-time `&'static str` constants so they can be used
//! in `const` contexts and compared by identity in tests.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Pattern every code MUST match: `E-<AREA>-<DETAIL>` or `W-<AREA>-<DETAIL>`.
pub const CODE_PATTERN_HINT: &str = "^[EW]-[A-Z]+-[A-Z]+$";

/// Validate a code string against `CODE_PATTERN_HINT` (SPEC §6.2 envelope).
#[must_use]
pub fn is_valid_code(code: &str) -> bool {
    let parts: Vec<&str> = code.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    if parts[0] != "E" && parts[0] != "W" {
        return false;
    }
    let letters = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_uppercase());
    letters(parts[1]) && letters(parts[2])
}

/// A Cairn error: stable code + human message + optional model/user recovery hint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct CairnError {
    pub code: &'static str,
    pub message: String,
    /// Guidance returned to the model in `error.recovery` (SPEC §6.2) and shown
    /// as the hint line in the TUI (SPEC §10.9).
    pub recovery: Option<String>,
    /// Underlying cause text (already redacted by the caller).
    pub cause: Option<String>,
}

impl CairnError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: None,
            cause: None,
        }
    }

    #[must_use]
    pub fn with_recovery(mut self, recovery: impl Into<String>) -> Self {
        self.recovery = Some(recovery.into());
        self
    }

    #[must_use]
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Model-visible shape (SPEC §6.2 `error` object).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code,
            "message": self.message,
            "recovery": self.recovery.clone().unwrap_or_default(),
        })
    }
}

/// Process exit statuses (SPEC §11.2). Values are stable across 1.x (REQ-CLI-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Ok = 0,
    Generic = 1,
    Usage = 2,
    Provider = 3,
    Guardrail = 4,
    Verify = 5,
    Permission = 6,
    Cancelled = 7,
    ApprovalRequired = 8,
    NotFound = 9,
    Busy = 10,
    Sandbox = 11,
    Flush = 13,
}

impl ExitStatus {
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::Generic => 1,
            Self::Usage => 2,
            Self::Provider => 3,
            Self::Guardrail => 4,
            Self::Verify => 5,
            Self::Permission => 6,
            Self::Cancelled => 7,
            Self::ApprovalRequired => 8,
            Self::NotFound => 9,
            Self::Busy => 10,
            Self::Sandbox => 11,
            Self::Flush => 13,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Generic => "ERR_GENERIC",
            Self::Usage => "ERR_USAGE",
            Self::Provider => "ERR_PROVIDER",
            Self::Guardrail => "ERR_GUARDRAIL",
            Self::Verify => "ERR_VERIFY",
            Self::Permission => "ERR_PERMISSION",
            Self::Cancelled => "ERR_CANCELLED",
            Self::ApprovalRequired => "ERR_APPROVAL_REQUIRED",
            Self::NotFound => "ERR_NOT_FOUND",
            Self::Busy => "ERR_BUSY",
            Self::Sandbox => "ERR_SANDBOX",
            Self::Flush => "ERR_FLUSH",
        }
    }

    /// Exit code 12 is reserved and MUST NOT be returned by 1.x (SPEC §11.2).
    pub const RESERVED_CODE: i32 = 12;
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name(), self.code())
    }
}

/// The stable code registry (SPEC §0, §4.5, §6.2, §11.4.2, §12.5, §14.3.2b).
///
/// Every `E-*`/`W-*` string that appears in SPEC.md lives here exactly once —
/// the list is generated from the spec by the M0 audit, so a code cannot be
/// documented and unregistered (or vice versa).
pub mod codes {
    // --- ASK ---
    pub const ASK_NOINPUT: &str = "E-ASK-NOINPUT";
    pub const ASK_TIMEOUT: &str = "E-ASK-TIMEOUT";

    // --- CFG ---
    pub const CFG_BADENV: &str = "E-CFG-BADENV";
    pub const CFG_BADGLOB: &str = "E-CFG-BADGLOB";
    pub const CFG_BADPATH: &str = "E-CFG-BADPATH";
    pub const CFG_BADREGEX: &str = "E-CFG-BADREGEX";
    pub const CFG_BADVALUE: &str = "E-CFG-BADVALUE";
    pub const CFG_DUPNAME: &str = "E-CFG-DUPNAME";
    pub const CFG_KEYCONFLICT: &str = "E-CFG-KEYCONFLICT";
    pub const CFG_KEYRESERVED: &str = "E-CFG-KEYRESERVED";
    pub const CFG_NOMODEL: &str = "E-CFG-NOMODEL";
    pub const CFG_RANGE: &str = "E-CFG-RANGE";
    pub const CFG_SUM: &str = "E-CFG-SUM";
    pub const CFG_THEME: &str = "E-CFG-THEME";
    pub const CFG_UNKNOWN: &str = "E-CFG-UNKNOWN";
    pub const CFG_UNSAFE_BLOCKED: &str = "E-CFG-UNSAFEBLOCKED";
    pub const CFG_UNSAFEREDACT: &str = "E-CFG-UNSAFEREDACT";
    pub const CFG_VERSION: &str = "E-CFG-VERSION";
    pub const CFG_FALLBACK: &str = "W-CFG-FALLBACK";
    pub const CFG_PARTIAL: &str = "W-CFG-PARTIAL";

    // --- CHK ---
    pub const CHK_DISK: &str = "E-CHK-DISK";
    pub const CHK_FAIL: &str = "E-CHK-FAIL";
    pub const CHK_HASH: &str = "E-CHK-HASH";
    pub const CHK_MERGE: &str = "E-CHK-MERGE";
    pub const W_CHK_FAIL: &str = "W-CHK-FAIL";

    // --- CLI ---
    pub const CLI_USAGE: &str = "E-CLI-USAGE";

    // --- CRED ---
    pub const CRED_PERM: &str = "W-CRED-PERM";

    // --- CTX ---
    pub const CTX_COMPACT: &str = "E-CTX-COMPACT";
    pub const CTX_NOVISION: &str = "E-CTX-NOVISION";
    pub const CTX_PINFULL: &str = "E-CTX-PINFULL";
    pub const CTX_ALIAS: &str = "W-CTX-ALIAS";
    pub const CTX_SYSPROMPT: &str = "W-CTX-SYSPROMPT";

    // --- DISC ---
    pub const DISC_CAP: &str = "W-DISC-CAP";
    pub const DISC_SYMLINK: &str = "W-DISC-SYMLINK";

    // --- EDIT ---
    pub const EDIT_AMBIGUOUS: &str = "E-EDIT-AMBIGUOUS";
    pub const EDIT_CONFLICT: &str = "E-EDIT-CONFLICT";
    pub const EDIT_NOCHANGE: &str = "E-EDIT-NOCHANGE";
    pub const EDIT_NOMATCH: &str = "E-EDIT-NOMATCH";
    pub const EDIT_PARTIAL: &str = "E-EDIT-PARTIAL";
    pub const EDIT_STALE: &str = "E-EDIT-STALE";
    pub const EDIT_SYNTAX: &str = "E-EDIT-SYNTAX";
    pub const EDIT_FUZZY: &str = "W-EDIT-FUZZY";

    // --- FS ---
    pub const FS_BADPATH: &str = "E-FS-BADPATH";
    pub const FS_BINARY: &str = "E-FS-BINARY";
    pub const FS_DIR: &str = "E-FS-DIR";
    pub const FS_DIRTY: &str = "E-FS-DIRTY";
    pub const FS_ENCODING: &str = "E-FS-ENCODING";
    pub const FS_ESCAPE: &str = "E-FS-ESCAPE";
    pub const FS_IGNORED: &str = "E-FS-IGNORED";
    pub const FS_NOPARENT: &str = "E-FS-NOPARENT";
    pub const FS_NOTFOUND: &str = "E-FS-NOTFOUND";
    pub const FS_PERM: &str = "E-FS-PERM";
    pub const FS_PROTECTED: &str = "E-FS-PROTECTED";
    pub const FS_READONLY: &str = "E-FS-READONLY";
    pub const FS_STALE: &str = "E-FS-STALE";
    pub const FS_TOOBIG: &str = "E-FS-TOOBIG";

    // --- GIT ---
    pub const GIT_BADREV: &str = "E-GIT-BADREV";
    pub const GIT_CMD: &str = "E-GIT-CMD";
    pub const GIT_CONFLICT: &str = "E-GIT-CONFLICT";
    pub const GIT_EMPTY: &str = "E-GIT-EMPTY";
    pub const GIT_LOCK: &str = "E-GIT-LOCK";
    pub const GIT_NOCFG: &str = "E-GIT-NOCFG";
    pub const GIT_NODIFF: &str = "E-GIT-NODIFF";
    pub const GIT_NOREPO: &str = "E-GIT-NOREPO";
    pub const GIT_PRECOMMIT: &str = "E-GIT-PRECOMMIT";

    // --- GLOB ---
    pub const GLOB_CAP: &str = "E-GLOB-CAP";
    pub const GLOB_SYNTAX: &str = "E-GLOB-SYNTAX";

    // --- GREP ---
    pub const GREP_CAP: &str = "E-GREP-CAP";
    pub const GREP_WALK: &str = "E-GREP-WALK";

    // --- HOOK ---
    pub const HOOK_BLOCKED: &str = "E-HOOK-BLOCKED";
    pub const HOOK_FAILED: &str = "W-HOOK-FAILED";

    // --- IDX ---
    pub const IDX_STORM: &str = "W-IDX-STORM";

    // --- IMPL ---
    pub const IMPL_STAGE: &str = "E-IMPL-STAGE";

    // --- INJ ---
    pub const INJ_OBSCURE: &str = "W-INJ-OBSCURE";

    // --- JOB ---
    pub const JOB_LIMIT: &str = "E-JOB-LIMIT";
    pub const JOB_NOTFOUND: &str = "E-JOB-NOTFOUND";

    // --- LOOP ---
    pub const LOOP_INVARIANT: &str = "E-LOOP-INVARIANT";
    pub const LOOP_MAXTOKENS: &str = "E-LOOP-MAXTOKENS";
    pub const ARCH_TRANSITION: &str = "E-LOOP-TRANSITION";
    pub const LOOP_VERIFY: &str = "E-LOOP-VERIFY";

    // --- MCP ---
    pub const MCP_CONNECT: &str = "E-MCP-CONNECT";
    pub const MCP_DENIED: &str = "E-MCP-DENIED";
    pub const MCP_DOWN: &str = "E-MCP-DOWN";
    pub const MCP_PROTO: &str = "E-MCP-PROTO";
    pub const MCP_TIMEOUT: &str = "E-MCP-TIMEOUT";
    pub const MCP_TOOLERR: &str = "E-MCP-TOOLERR";

    // --- MODE ---
    pub const MODE_FIXED: &str = "W-MODE-FIXED";

    // --- ORPHAN ---
    pub const ORPHAN_KILLED: &str = "E-ORPHAN-KILLED";

    // --- PARSE ---
    pub const PARSE_GRAMMAR: &str = "W-PARSE-GRAMMAR";

    // --- PERF ---
    pub const PERF_MEM: &str = "E-PERF-MEM";

    // --- PERM ---
    pub const PERM_BADPARSE: &str = "E-PERM-BADPARSE";
    pub const PERM_CHAIN: &str = "E-PERM-CHAIN";
    pub const PERM_DENIED: &str = "E-PERM-DENIED";
    pub const PERM_MODE: &str = "E-PERM-MODE";
    pub const PERM_TIMEOUT: &str = "E-PERM-TIMEOUT";
    pub const PERM_BADREGEX: &str = "W-PERM-BADREGEX";

    // --- PLAN ---
    pub const PLAN_DRIFT: &str = "E-PLAN-DRIFT";
    pub const PLAN_INVALID: &str = "E-PLAN-INVALID";
    pub const PLAN_MDREGEN: &str = "W-PLAN-MDREGEN";
    pub const PLAN_TODODRIFT: &str = "W-PLAN-TODODRIFT";

    // --- PROV ---
    pub const PROV_AUTH: &str = "E-PROV-AUTH";
    pub const PROV_CONTEXT: &str = "E-PROV-CONTEXT";
    pub const PROV_EVENTBIG: &str = "E-PROV-EVENTBIG";
    pub const PROV_FALLBACK: &str = "E-PROV-FALLBACK";
    pub const PROV_FILTER: &str = "E-PROV-FILTER";
    pub const PROV_FORBID: &str = "E-PROV-FORBID";
    pub const PROV_IDLE: &str = "E-PROV-IDLE";
    pub const PROV_MALFORMED: &str = "E-PROV-MALFORMED";
    pub const PROV_NET: &str = "E-PROV-NET";
    pub const PROV_NOMODEL: &str = "E-PROV-NOMODEL";
    pub const PROV_OFFLINE: &str = "E-PROV-OFFLINE";
    pub const PROV_PAYLOAD: &str = "E-PROV-PAYLOAD";
    pub const PROV_PROTO: &str = "E-PROV-PROTO";
    pub const PROV_RATELIMIT: &str = "E-PROV-RATELIMIT";
    pub const PROV_REQ: &str = "E-PROV-REQ";
    pub const PROV_SERVER: &str = "E-PROV-SERVER";
    pub const PROV_TIMEOUT: &str = "E-PROV-TIMEOUT";
    pub const PROV_TLS: &str = "E-PROV-TLS";

    // --- REGEX ---
    pub const REGEX_SYNTAX: &str = "E-REGEX-SYNTAX";
    pub const REGEX_TOOBIG: &str = "E-REGEX-TOOBIG";

    // --- SANDBOX ---
    pub const SANDBOX_DENY: &str = "E-SANDBOX-DENY";
    pub const SANDBOX_ADVISORY: &str = "W-SANDBOX-ADVISORY";

    // --- SESS ---
    pub const SESS_NOTFOUND: &str = "E-SESS-NOTFOUND";

    // --- SHELL ---
    pub const SHELL_EXITNONZERO: &str = "E-SHELL-EXITNONZERO";
    pub const SHELL_NOEXEC: &str = "E-SHELL-NOEXEC";
    pub const SHELL_PTY: &str = "E-SHELL-PTY";
    pub const SHELL_TIMEOUT: &str = "E-SHELL-TIMEOUT";
    pub const SHELL_TOOBIG: &str = "E-SHELL-TOOBIG";
    pub const SHELL_DISCARDED: &str = "W-SHELL-DISCARDED";

    // --- STATE ---
    pub const STATE_PERM: &str = "E-STATE-PERM";

    // --- SUB ---
    pub const SUB_DEPTH: &str = "E-SUB-DEPTH";
    pub const SUB_DISABLED: &str = "E-SUB-DISABLED";
    pub const SUB_FAILED: &str = "E-SUB-FAILED";
    pub const SUB_TIMEOUT: &str = "E-SUB-TIMEOUT";
    pub const SUB_TOOLS: &str = "E-SUB-TOOLS";

    // --- TODO ---
    pub const TODO_STATUS: &str = "E-TODO-STATUS";

    // --- TOOL ---
    pub const TOOL_BADJSON: &str = "E-TOOL-BADJSON";
    pub const TOOL_BADSCHEMA: &str = "E-TOOL-BADSCHEMA";
    pub const TOOL_CANCELLED: &str = "E-TOOL-CANCELLED";
    pub const TOOL_TIMEOUT: &str = "E-TOOL-TIMEOUT";
    pub const TOOL_TOOBIG: &str = "E-TOOL-TOOBIG";
    pub const TOOL_BURST: &str = "W-TOOL-BURST";

    // --- TUI ---
    pub const TUI_CONHOST: &str = "W-TUI-CONHOST";

    // --- UPDATE ---
    pub const UPDATE_SIGNATURE: &str = "E-UPDATE-SIGNATURE";

    // --- WEB ---
    pub const WEB_DNS: &str = "E-WEB-DNS";
    pub const WEB_EXFIL: &str = "E-WEB-EXFIL";
    pub const WEB_REDIRECTS: &str = "E-WEB-REDIRECTS";
    pub const WEB_SCHEME: &str = "E-WEB-SCHEME";
    pub const WEB_SSRF: &str = "E-WEB-SSRF";
    pub const WEB_STATUS: &str = "E-WEB-STATUS";
    pub const WEB_TIMEOUT: &str = "E-WEB-TIMEOUT";
    pub const WEB_TLS: &str = "E-WEB-TLS";
    pub const WEB_TOOBIG: &str = "E-WEB-TOOBIG";
}

/// Every code in the registry — asserted valid by `codes_match_pattern` (T-CLI-002 support).
pub const ALL_CODES: &[&str] = &[
    codes::ASK_NOINPUT,
    codes::ASK_TIMEOUT,
    codes::CFG_BADENV,
    codes::CFG_BADGLOB,
    codes::CFG_BADPATH,
    codes::CFG_BADREGEX,
    codes::CFG_BADVALUE,
    codes::CFG_DUPNAME,
    codes::CFG_KEYCONFLICT,
    codes::CFG_KEYRESERVED,
    codes::CFG_NOMODEL,
    codes::CFG_RANGE,
    codes::CFG_SUM,
    codes::CFG_THEME,
    codes::CFG_UNKNOWN,
    codes::CFG_UNSAFE_BLOCKED,
    codes::CFG_UNSAFEREDACT,
    codes::CFG_VERSION,
    codes::CHK_DISK,
    codes::CHK_FAIL,
    codes::CHK_HASH,
    codes::CHK_MERGE,
    codes::CLI_USAGE,
    codes::CTX_COMPACT,
    codes::CTX_NOVISION,
    codes::CTX_PINFULL,
    codes::EDIT_AMBIGUOUS,
    codes::EDIT_CONFLICT,
    codes::EDIT_NOCHANGE,
    codes::EDIT_NOMATCH,
    codes::EDIT_PARTIAL,
    codes::EDIT_STALE,
    codes::EDIT_SYNTAX,
    codes::FS_BADPATH,
    codes::FS_BINARY,
    codes::FS_DIR,
    codes::FS_DIRTY,
    codes::FS_ENCODING,
    codes::FS_ESCAPE,
    codes::FS_IGNORED,
    codes::FS_NOPARENT,
    codes::FS_NOTFOUND,
    codes::FS_PERM,
    codes::FS_PROTECTED,
    codes::FS_READONLY,
    codes::FS_STALE,
    codes::FS_TOOBIG,
    codes::GIT_BADREV,
    codes::GIT_CMD,
    codes::GIT_CONFLICT,
    codes::GIT_EMPTY,
    codes::GIT_LOCK,
    codes::GIT_NOCFG,
    codes::GIT_NODIFF,
    codes::GIT_NOREPO,
    codes::GIT_PRECOMMIT,
    codes::GLOB_CAP,
    codes::GLOB_SYNTAX,
    codes::GREP_CAP,
    codes::GREP_WALK,
    codes::HOOK_BLOCKED,
    codes::IMPL_STAGE,
    codes::JOB_LIMIT,
    codes::JOB_NOTFOUND,
    codes::LOOP_INVARIANT,
    codes::LOOP_MAXTOKENS,
    codes::ARCH_TRANSITION,
    codes::LOOP_VERIFY,
    codes::MCP_CONNECT,
    codes::MCP_DENIED,
    codes::MCP_DOWN,
    codes::MCP_PROTO,
    codes::MCP_TIMEOUT,
    codes::MCP_TOOLERR,
    codes::ORPHAN_KILLED,
    codes::PERF_MEM,
    codes::PERM_BADPARSE,
    codes::PERM_CHAIN,
    codes::PERM_DENIED,
    codes::PERM_MODE,
    codes::PERM_TIMEOUT,
    codes::PLAN_DRIFT,
    codes::PLAN_INVALID,
    codes::PROV_AUTH,
    codes::PROV_CONTEXT,
    codes::PROV_EVENTBIG,
    codes::PROV_FALLBACK,
    codes::PROV_FILTER,
    codes::PROV_FORBID,
    codes::PROV_IDLE,
    codes::PROV_MALFORMED,
    codes::PROV_NET,
    codes::PROV_NOMODEL,
    codes::PROV_OFFLINE,
    codes::PROV_PAYLOAD,
    codes::PROV_PROTO,
    codes::PROV_RATELIMIT,
    codes::PROV_REQ,
    codes::PROV_SERVER,
    codes::PROV_TIMEOUT,
    codes::PROV_TLS,
    codes::REGEX_SYNTAX,
    codes::REGEX_TOOBIG,
    codes::SANDBOX_DENY,
    codes::SESS_NOTFOUND,
    codes::SHELL_EXITNONZERO,
    codes::SHELL_NOEXEC,
    codes::SHELL_PTY,
    codes::SHELL_TIMEOUT,
    codes::SHELL_TOOBIG,
    codes::STATE_PERM,
    codes::SUB_DEPTH,
    codes::SUB_DISABLED,
    codes::SUB_FAILED,
    codes::SUB_TIMEOUT,
    codes::SUB_TOOLS,
    codes::TODO_STATUS,
    codes::TOOL_BADJSON,
    codes::TOOL_BADSCHEMA,
    codes::TOOL_CANCELLED,
    codes::TOOL_TIMEOUT,
    codes::TOOL_TOOBIG,
    codes::UPDATE_SIGNATURE,
    codes::WEB_DNS,
    codes::WEB_EXFIL,
    codes::WEB_REDIRECTS,
    codes::WEB_SCHEME,
    codes::WEB_SSRF,
    codes::WEB_STATUS,
    codes::WEB_TIMEOUT,
    codes::WEB_TLS,
    codes::WEB_TOOBIG,
    codes::CFG_FALLBACK,
    codes::CFG_PARTIAL,
    codes::W_CHK_FAIL,
    codes::CRED_PERM,
    codes::CTX_ALIAS,
    codes::CTX_SYSPROMPT,
    codes::DISC_CAP,
    codes::DISC_SYMLINK,
    codes::EDIT_FUZZY,
    codes::HOOK_FAILED,
    codes::IDX_STORM,
    codes::INJ_OBSCURE,
    codes::MODE_FIXED,
    codes::PARSE_GRAMMAR,
    codes::PERM_BADREGEX,
    codes::PLAN_MDREGEN,
    codes::PLAN_TODODRIFT,
    codes::SANDBOX_ADVISORY,
    codes::SHELL_DISCARDED,
    codes::TOOL_BURST,
    codes::TUI_CONHOST,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_pattern() {
        for code in ALL_CODES {
            assert!(
                is_valid_code(code),
                "invalid code {code} (pattern {CODE_PATTERN_HINT})"
            );
        }
    }

    #[test]
    fn pattern_rejects_bad_shapes() {
        for bad in [
            "",
            "E",
            "E-",
            "e-fs-notfound",
            "E-fs-NOTFOUND",
            "E-FS",
            "X-FS-OK",
            "E-FS-",
            "E-FS-ok",
            "EF-OK",
            "E-FS-A-B",
            "E-FS-A-B-C",
        ] {
            assert!(!is_valid_code(bad), "expected invalid: {bad:?}");
        }
    }

    #[test]
    fn exit_codes_match_spec_table() {
        assert_eq!(ExitStatus::Ok.code(), 0);
        assert_eq!(ExitStatus::Generic.code(), 1);
        assert_eq!(ExitStatus::Usage.code(), 2);
        assert_eq!(ExitStatus::Provider.code(), 3);
        assert_eq!(ExitStatus::Guardrail.code(), 4);
        assert_eq!(ExitStatus::Verify.code(), 5);
        assert_eq!(ExitStatus::Permission.code(), 6);
        assert_eq!(ExitStatus::Cancelled.code(), 7);
        assert_eq!(ExitStatus::ApprovalRequired.code(), 8);
        assert_eq!(ExitStatus::NotFound.code(), 9);
        assert_eq!(ExitStatus::Busy.code(), 10);
        assert_eq!(ExitStatus::Sandbox.code(), 11);
        assert_eq!(ExitStatus::Flush.code(), 13);
        assert_eq!(ExitStatus::RESERVED_CODE, 12);
        let mut seen = std::collections::BTreeSet::new();
        for s in [
            ExitStatus::Ok,
            ExitStatus::Generic,
            ExitStatus::Usage,
            ExitStatus::Provider,
            ExitStatus::Guardrail,
            ExitStatus::Verify,
            ExitStatus::Permission,
            ExitStatus::Cancelled,
            ExitStatus::ApprovalRequired,
            ExitStatus::NotFound,
            ExitStatus::Busy,
            ExitStatus::Sandbox,
            ExitStatus::Flush,
        ] {
            assert!(seen.insert(s.code()), "duplicate exit code {}", s.code());
        }
    }

    #[test]
    fn error_json_is_model_visible_shape() {
        let e = CairnError::new(codes::FS_NOTFOUND, "no such file").with_recovery("check path");
        let v = e.to_json();
        assert_eq!(v["code"], "E-FS-NOTFOUND");
        assert_eq!(v["message"], "no such file");
        assert_eq!(v["recovery"], "check path");
        assert_eq!(e.to_string(), "E-FS-NOTFOUND: no such file");
    }
}
