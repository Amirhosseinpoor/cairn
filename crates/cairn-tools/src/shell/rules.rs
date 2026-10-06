//! §9.3.1's denylist and §9.3.2's gates, one command at a time.
//!
//! Everything here looks at a single resolved command (`name` and `args`) and
//! says what is wrong with it, if anything. How commands are found, nested and
//! chained is `shell/mod.rs`'s concern.

use std::path::{Component, Path, PathBuf};

use cairn_parse::shell::Word;

use super::Ctx;
use crate::types::Access;

/// One resolved command.
#[derive(Debug, Clone)]
pub struct Exec {
    /// The program, path stripped when it was absolute.
    pub name: String,
    /// The program as it was written, for matching rules.
    pub raw_name: String,
    /// Written with a relative path (`./sudo`): a file in the workspace, not
    /// the system program of that name.
    pub local: bool,
    pub args: Vec<Word>,
    /// `{}` in these arguments was filled in from a `find` start path.
    pub substituted: bool,
    /// A wrapper (`env`, `nohup`, `sh -c`, …) that only runs what follows:
    /// its own findings carry over to that command and it is not a leaf.
    pub transparent: bool,
}

/// What a command is, as far as the policy cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A denylist hit (rule number, reason).
    Deny(u8, String),
    /// Ask everywhere but `auto-unsafe`.
    AskUnlessUnsafe(String),
    /// Plan denies, everything else asks, nothing pre-approves.
    AskAlways(String),
}

const PRIVILEGE: [&str; 5] = ["sudo", "doas", "pkexec", "su", "runas"];
const DISK: [&str; 8] = [
    "fdisk",
    "sfdisk",
    "cfdisk",
    "gdisk",
    "parted",
    "wipefs",
    "mkswap",
    "blkdiscard",
];
const INTERPRETERS: [&str; 14] = [
    "sh", "bash", "zsh", "dash", "ksh", "ash", "fish", "csh", "tcsh", "perl", "ruby", "node",
    "nodejs", "php",
];
const FETCHERS: [&str; 8] = [
    "curl",
    "wget",
    "fetch",
    "http",
    "https",
    "aria2c",
    "lwp-download",
    "xh",
];
const NETWORK: [&str; 17] = [
    "curl", "wget", "http", "https", "httpie", "nc", "ncat", "netcat", "ssh", "scp", "sftp",
    "rsync", "ftp", "telnet", "dig", "nslookup", "ping",
];
const REMOTE_SHELL: [&str; 4] = ["ssh", "scp", "sftp", "rsync"];
const SYSTEM_PROCS: [&str; 8] = [
    "sshd",
    "init",
    "systemd",
    "dbus-daemon",
    "login",
    "launchd",
    "kernel_task",
    "systemd-logind",
];

/// Names that write to the paths they are given.
const WRITERS: [&str; 17] = [
    "cp", "mv", "tee", "ln", "install", "touch", "chmod", "chown", "chgrp", "rm", "truncate",
    "rsync", "mkdir", "rmdir", "dd", "shred", "unlink",
];
/// Names that read the paths they are given and print them.
const READERS: [&str; 13] = [
    "cat", "head", "tail", "less", "more", "tac", "strings", "xxd", "od", "base64", "source", ".",
    "nl",
];

pub fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.contains(&name)
        || name
            .strip_prefix("python")
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

pub fn is_fetcher(name: &str) -> bool {
    FETCHERS.contains(&name)
}

pub fn is_network(name: &str) -> bool {
    NETWORK.contains(&name)
}

/// Words that are options (start with `-`), with `--` ending them.
fn split_args(args: &[Word]) -> (Vec<&str>, Vec<&Word>) {
    let mut flags = Vec::new();
    let mut rest = Vec::new();
    let mut options_over = false;
    for word in args {
        if !options_over && word.text == "--" {
            options_over = true;
        } else if !options_over && word.text.starts_with('-') && word.text.len() > 1 {
            flags.push(word.text.as_str());
        } else {
            rest.push(word);
        }
    }
    (flags, rest)
}

fn has_short(flags: &[&str], letters: &[char]) -> bool {
    flags
        .iter()
        .any(|f| !f.starts_with("--") && f.chars().skip(1).any(|c| letters.contains(&c)))
}

fn has_long(flags: &[&str], names: &[&str]) -> bool {
    flags.iter().any(|f| names.contains(f))
}

/// `~`, `$HOME` and `${HOME}` at the start of a word, expanded.
pub fn expand_home(text: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return text.to_string();
    };
    for prefix in ["${HOME}", "$HOME", "~"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("{home}{rest}");
            }
        }
    }
    text.to_string()
}

/// A target with a variable left in it cannot be judged.
fn unresolved(text: &str) -> bool {
    text.contains('$') || text.contains('`')
}

fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// An absolute, lexically clean path for a command-line target.
pub fn target_path(ctx: &Ctx<'_>, text: &str) -> Option<PathBuf> {
    let expanded = expand_home(text, ctx.home);
    if unresolved(&expanded) || expanded.is_empty() {
        return None;
    }
    let path = Path::new(&expanded);
    Some(normalized(&if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    }))
}

/// Whether deleting or rewriting everything under `path` would be a disaster:
/// the filesystem root, the home directory, or any directory that holds the
/// workspace.
fn catastrophic(ctx: &Ctx<'_>, path: &Path) -> bool {
    let root = ctx.boundary.root();
    path.parent().is_none()
        || ctx.home.is_some_and(|h| normalized(Path::new(h)) == path)
        || (path != root && root.starts_with(path))
}

fn strip_glob_tail(text: &str) -> &str {
    for tail in ["/*", "/.*", "/."] {
        if let Some(head) = text.strip_suffix(tail) {
            return if head.is_empty() { "/" } else { head };
        }
    }
    text
}

fn rm(exec: &Exec, ctx: &Ctx<'_>) -> Option<Finding> {
    let (flags, targets) = split_args(&exec.args);
    let recursive = has_short(&flags, &['r', 'R']) || has_long(&flags, &["--recursive"]);
    let force = has_short(&flags, &['f']) || has_long(&flags, &["--force"]);
    for word in &targets {
        let text = strip_glob_tail(&word.text);
        let lowered = word.text.to_ascii_lowercase();
        if lowered.ends_with("/.bash_history") || lowered.ends_with("/.zsh_history") {
            return Some(Finding::Deny(10, "deletes shell history".to_string()));
        }
        let Some(path) = target_path(ctx, text) else {
            continue;
        };
        if path.starts_with("/var/spool/cron") {
            return Some(Finding::Deny(18, "deletes scheduled jobs".to_string()));
        }
        if !recursive {
            continue;
        }
        if catastrophic(ctx, &path) {
            return Some(Finding::Deny(
                if force { 2 } else { 3 },
                format!(
                    "`rm -r` on `{}` would delete far more than the project",
                    word.text
                ),
            ));
        }
        if force && path == ctx.boundary.root() && !exec.substituted && text == word.text {
            return Some(Finding::Deny(
                2,
                "`rm -rf .` would delete the whole workspace".to_string(),
            ));
        }
    }
    None
}

fn recursive_permission(exec: &Exec, ctx: &Ctx<'_>) -> Option<Finding> {
    let (flags, rest) = split_args(&exec.args);
    if !(has_short(&flags, &['R']) || has_long(&flags, &["--recursive"])) {
        return None;
    }
    // The first operand is the mode or the owner.
    for word in rest.iter().skip(1) {
        let Some(path) = target_path(ctx, strip_glob_tail(&word.text)) else {
            continue;
        };
        let outside = !path.starts_with(ctx.boundary.root());
        if exec.name == "chmod" && catastrophic(ctx, &path) {
            return Some(Finding::Deny(
                6,
                "recursive chmod on a system directory".to_string(),
            ));
        }
        if exec.name != "chmod" && outside {
            return Some(Finding::Deny(
                6,
                format!("recursive {} outside the workspace", exec.name),
            ));
        }
    }
    None
}

fn git_subcommand(args: &[Word]) -> Option<(usize, &str)> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].text.as_str();
        if matches!(a, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some((i, a));
        }
    }
    None
}

fn git(exec: &Exec) -> Option<Finding> {
    let (at, sub) = git_subcommand(&exec.args)?;
    let rest: Vec<&str> = exec.args[at + 1..]
        .iter()
        .map(|w| w.text.as_str())
        .collect();
    match sub {
        "reset" if rest.contains(&"--hard") => Some(Finding::AskAlways(
            "`git reset --hard` discards uncommitted work".to_string(),
        )),
        "checkout" if rest.contains(&"--") => Some(Finding::AskAlways(
            "`git checkout --` discards uncommitted changes".to_string(),
        )),
        "push"
            if rest.iter().any(|a| {
                matches!(*a, "-f" | "--force" | "--force-with-lease")
                    || a.starts_with("--force-with-lease=")
                    || a.starts_with('+')
            }) =>
        {
            Some(Finding::AskUnlessUnsafe(
                "force-pushing rewrites remote history".to_string(),
            ))
        }
        _ => None,
    }
}

fn kill(exec: &Exec) -> Option<Finding> {
    let words: Vec<&str> = exec.args.iter().map(|w| w.text.as_str()).collect();
    match exec.name.as_str() {
        "kill" if words.contains(&"-1") || words.last() == Some(&"1") => Some(Finding::Deny(
            12,
            "signals every process (or init)".to_string(),
        )),
        "killall" | "pkill" => words
            .iter()
            .find(|w| !w.starts_with('-') && SYSTEM_PROCS.contains(w))
            .map(|w| Finding::Deny(12, format!("kills the system process `{w}`"))),
        _ => None,
    }
}

fn systemctl(exec: &Exec) -> Option<Finding> {
    let verb = exec
        .args
        .iter()
        .map(|w| w.text.as_str())
        .find(|w| !w.starts_with('-'))?;
    matches!(
        verb,
        "stop"
            | "disable"
            | "mask"
            | "isolate"
            | "poweroff"
            | "reboot"
            | "halt"
            | "kexec"
            | "rescue"
            | "emergency"
    )
    .then(|| Finding::Deny(14, format!("`systemctl {verb}` disrupts services")))
}

fn firewall_and_infra(exec: &Exec) -> Option<Finding> {
    let words: Vec<&str> = exec.args.iter().map(|w| w.text.as_str()).collect();
    let deny = |n: u8, why: &str| Some(Finding::Deny(n, why.to_string()));
    match exec.name.as_str() {
        "iptables" | "ip6tables" if words.iter().any(|w| matches!(*w, "-F" | "--flush")) => {
            deny(17, "flushes the firewall")
        }
        "nft" if words.first() == Some(&"flush") => deny(17, "flushes the firewall"),
        "ufw" if matches!(words.first(), Some(&"disable" | &"reset")) => {
            deny(17, "disables the firewall")
        }
        "crontab" if words.contains(&"-r") => deny(18, "removes every scheduled job"),
        "docker" if words.first() == Some(&"system") && words.get(1) == Some(&"prune") => {
            let (flags, _) = split_args(&exec.args[2..]);
            let all = has_short(&flags, &['a']) || has_long(&flags, &["--all"]);
            let force = has_short(&flags, &['f']) || has_long(&flags, &["--force"]);
            (all && force).then(|| Finding::Deny(15, "prunes every docker object".to_string()))
        }
        "kubectl" if words.first() == Some(&"delete") => words
            .get(1)
            .filter(|w| {
                matches!(
                    **w,
                    "namespace"
                        | "namespaces"
                        | "ns"
                        | "node"
                        | "nodes"
                        | "persistentvolume"
                        | "persistentvolumes"
                        | "pv"
                )
            })
            .map(|w| Finding::Deny(16, format!("`kubectl delete {w}` damages the cluster"))),
        _ => None,
    }
}

fn disk(exec: &Exec, ctx: &Ctx<'_>) -> Option<Finding> {
    let name = exec.name.as_str();
    if name.starts_with("mkfs") || DISK.contains(&name) {
        return Some(Finding::Deny(4, format!("`{name}` destroys disks")));
    }
    if name == "dd" {
        let to_device = exec.args.iter().any(|w| {
            w.text
                .strip_prefix("of=")
                .is_some_and(|t| t.starts_with("/dev/") && t != "/dev/null")
        });
        if to_device {
            return Some(Finding::Deny(4, "`dd` writing to a device".to_string()));
        }
    }
    if name == "shred" {
        let (_, targets) = split_args(&exec.args);
        let outside = targets.iter().any(|w| {
            target_path(ctx, &w.text).is_some_and(|p| !p.starts_with(ctx.boundary.root()))
        });
        if outside {
            return Some(Finding::Deny(
                4,
                "`shred` outside the workspace".to_string(),
            ));
        }
    }
    None
}

/// Whether any word is a `/dev/tcp` or `/dev/udp` path.
pub fn dev_net(text: &str) -> bool {
    text.contains("/dev/tcp/") || text.contains("/dev/udp/")
}

fn reverse_shell(exec: &Exec) -> Option<Finding> {
    let words: Vec<&str> = exec.args.iter().map(|w| w.text.as_str()).collect();
    if matches!(exec.name.as_str(), "nc" | "ncat" | "netcat")
        && words
            .iter()
            .any(|w| matches!(*w, "-e" | "-c" | "--exec" | "--sh-exec" | "--lua-exec"))
    {
        return Some(Finding::Deny(11, "a reverse shell".to_string()));
    }
    exec.args
        .iter()
        .any(|w| dev_net(&w.text))
        .then(|| Finding::Deny(11, "opens a network socket through /dev/tcp".to_string()))
}

fn power(exec: &Exec) -> Option<Finding> {
    let name = exec.name.as_str();
    let off = matches!(name, "shutdown" | "reboot" | "halt" | "poweroff")
        || (matches!(name, "init" | "telinit")
            && exec
                .args
                .first()
                .is_some_and(|w| matches!(w.text.as_str(), "0" | "6")));
    off.then(|| Finding::Deny(13, "takes the machine down".to_string()))
}

fn history(exec: &Exec) -> Option<Finding> {
    let words: Vec<&str> = exec.args.iter().map(|w| w.text.as_str()).collect();
    ((exec.name == "history" && words.contains(&"-c"))
        || (exec.name == "unset" && words.contains(&"HISTFILE")))
    .then(|| Finding::Deny(10, "erases shell history".to_string()))
}

fn python_obfuscation(exec: &Exec) -> Option<Finding> {
    let is_py = exec.name.starts_with("python");
    if !(is_py || exec.name == "perl" || exec.name == "ruby" || exec.name == "node") {
        return None;
    }
    let code = exec
        .args
        .iter()
        .map(|w| w.raw.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (code.contains("base64") && (code.contains("exec(") || code.contains("eval(")))
        .then(|| Finding::AskAlways("runs obfuscated code".to_string()))
}

/// Findings for one command, independent of how it is chained.
pub fn check(exec: &Exec, ctx: &Ctx<'_>) -> Vec<Finding> {
    let mut found = Vec::new();
    if exec.local {
        // `./sudo` is a file in the workspace; it is whatever it is.
        return found;
    }
    let name = exec.name.as_str();
    if PRIVILEGE.contains(&name) {
        found.push(Finding::Deny(1, format!("`{name}` escalates privileges")));
    }
    let checks: [Option<Finding>; 11] = [
        (name == "rm").then(|| rm(exec, ctx)).flatten(),
        matches!(name, "chmod" | "chown" | "chgrp")
            .then(|| recursive_permission(exec, ctx))
            .flatten(),
        disk(exec, ctx),
        reverse_shell(exec),
        power(exec),
        history(exec),
        kill(exec),
        (name == "systemctl").then(|| systemctl(exec)).flatten(),
        firewall_and_infra(exec),
        (name == "git").then(|| git(exec)).flatten(),
        python_obfuscation(exec),
    ];
    found.extend(checks.into_iter().flatten());
    match name {
        "nohup" | "disown" => found.push(Finding::AskUnlessUnsafe(
            "leaves a process running after the session".to_string(),
        )),
        n if REMOTE_SHELL.contains(&n) => found.push(Finding::AskUnlessUnsafe(
            "talks to a remote host".to_string(),
        )),
        "telnet" => found.push(Finding::AskUnlessUnsafe(
            "talks to a remote host".to_string(),
        )),
        _ => {}
    }
    found
}

/// Whether a command that is on the read-only list could still change
/// something with the options it was given.
pub fn writes_despite_name(exec: &Exec) -> bool {
    let words: Vec<&str> = exec.args.iter().map(|w| w.text.as_str()).collect();
    let any = |names: &[&str]| words.iter().any(|w| names.contains(w));
    match exec.name.as_str() {
        "find" => any(&[
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        "git" => {
            let Some((at, sub)) = git_subcommand(&exec.args) else {
                return false;
            };
            let rest: Vec<&str> = exec.args[at + 1..]
                .iter()
                .map(|w| w.text.as_str())
                .collect();
            let output = rest.iter().any(|a| a.starts_with("--output"));
            match sub {
                "branch" => rest.iter().any(|a| {
                    matches!(
                        *a,
                        "-d" | "-D"
                            | "-m"
                            | "-M"
                            | "-c"
                            | "-C"
                            | "--delete"
                            | "--move"
                            | "--copy"
                            | "--set-upstream-to"
                            | "-u"
                            | "--unset-upstream"
                            | "--edit-description"
                    ) || !a.starts_with('-')
                }),
                "config" => !rest.contains(&"--get") && !rest.contains(&"--get-all"),
                "remote" => !(rest == ["-v"] || rest.is_empty()),
                "diff" | "log" | "show" => {
                    output || rest.iter().any(|a| a.starts_with("--ext-diff"))
                }
                _ => false,
            }
        }
        "sed" => {
            // Only the plain printing forms are read-only.
            static SAFE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let safe = SAFE.get_or_init(|| {
                regex::Regex::new(r"^((\d+|\$)(,(\d+|\$))?|/[^/]+/(,/[^/]+/)?)p$")
                    .expect("static regex")
            });
            let script = exec.args.iter().find(|w| !w.text.starts_with('-'));
            !script.is_some_and(|w| safe.is_match(&w.text))
                || words
                    .iter()
                    .any(|w| *w == "-i" || w.starts_with("-i") || *w == "--in-place")
        }
        "awk" | "gawk" | "mawk" => exec.args.iter().any(|w| {
            let t = w.text.as_str();
            t.contains("system(") || t.contains("getline") || t.contains('>') || t.contains('|')
        }),
        "tree" => words.contains(&"-o"),
        "rg" | "grep" | "egrep" | "fgrep" => words.iter().any(|w| w.starts_with("--pre")),
        _ => false,
    }
}

/// Targets of a command-line argument that deserve a boundary check, with the
/// access the command implies.
pub fn path_args(exec: &Exec) -> Vec<(&str, Access)> {
    let name = exec.name.as_str();
    let access = if WRITERS.contains(&name) {
        Access::Write
    } else if READERS.contains(&name) {
        Access::Read
    } else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for word in &exec.args {
        let text = word.text.as_str();
        if let Some(of) = text.strip_prefix("of=") {
            out.push((of, Access::Write));
        } else if let Some(value) = text.strip_prefix("--output=") {
            out.push((value, Access::Write));
        } else if !text.starts_with('-') {
            out.push((text, access));
        }
    }
    out
}
