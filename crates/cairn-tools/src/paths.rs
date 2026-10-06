//! The filesystem boundary (SPEC §9.4, §6.5 steps 3–5).
//!
//! A path arrives as text the model wrote. [`Boundary::resolve`] turns it
//! into one canonical absolute path — resolving `..` lexically first, then
//! following symlinks through the deepest part that exists — and only then
//! asks whether that path is somewhere a tool may go. Doing the symlink
//! resolution *before* the containment check is the whole point: a symlink
//! inside the workspace that points at `/etc` must be judged by where it
//! leads, not where it sits (REQ-CTX-003).
//!
//! Containment is not enough on its own: some places inside the allowed set
//! are still off limits (`.env`, `~/.ssh`, the repository's own `.git`).
//! Those are the *protected* paths, reported on [`Resolved::protected`] and
//! denied by the permission layer in every mode (D12).

use std::path::{Component, Path, PathBuf};

use cairn_core::error::codes;
use globset::{Glob, GlobBuilder, GlobMatcher};

use crate::types::{Access, ToolError};

/// An extra directory a session may use (`security.additional_dirs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    pub writable: bool,
}

/// One protected pattern. `writes_only` entries stay readable: `.git/**` must
/// be readable by the git tools, and Cairn's own `permissions.json` is worth
/// reading but never worth letting a model rewrite (REQ-SAFE-004).
#[derive(Debug, Clone)]
struct Protected {
    matcher: GlobMatcher,
    writes_only: bool,
}

/// A path a tool may touch, as the pipeline needs to describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Canonical absolute path (the file itself need not exist).
    pub abs: PathBuf,
    /// Workspace-relative, `/`-separated; `None` outside the workspace root
    /// (a granted additional directory).
    pub rel: Option<String>,
    /// §9.4's protected set matched (for this access).
    pub protected: bool,
    /// The path is a secret store (`.env`, keys, `~/.ssh`) whether or not the
    /// user lifted its protection: reading it taints the turn (REQ-SAFE-013).
    pub secret: bool,
}

impl Resolved {
    /// The spelling to show a model: workspace-relative when it can be.
    #[must_use]
    pub fn display(&self) -> String {
        self.rel.clone().unwrap_or_else(|| to_posix(&self.abs))
    }
}

/// Where a path would land, without refusing it (shell analysis wants the
/// facts, not an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub abs: PathBuf,
    /// In the workspace or a granted directory.
    pub inside: bool,
    /// §9.4's protected set matched (for this access).
    pub protected: bool,
}

/// The set of places tools may go, and the places they may not.
#[derive(Debug, Clone)]
pub struct Boundary {
    root: PathBuf,
    grants: Vec<Grant>,
    home: Option<PathBuf>,
    protected: Vec<Protected>,
    unprotected: Vec<GlobMatcher>,
}

fn to_posix(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // `canonicalize` on Windows yields `\\?\C:\…`; the prefix is noise to a
    // model and would defeat every glob below.
    text.strip_prefix("//?/")
        .map_or(text.clone(), str::to_string)
}

fn glob(pattern: &str, case_insensitive: bool) -> GlobMatcher {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(case_insensitive)
        .build()
        .unwrap_or_else(|_| Glob::new("\u{0}never-matches").expect("literal glob"))
        .compile_matcher()
}

/// `~`-abbreviate for messages.
fn abbreviate(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", to_posix(rest)),
        None => to_posix(path),
    }
}

impl Boundary {
    /// Build the boundary for a workspace.
    ///
    /// `additional` are `security.additional_dirs` (read-only unless
    /// `writable`); `allow_protected` are `security.allow_protected_paths`
    /// globs that lift individual protections.
    ///
    /// # Errors
    /// `E-FS-NOTFOUND` when the workspace root does not exist.
    pub fn new(
        root: &Path,
        additional: &[PathBuf],
        writable: bool,
        home: Option<PathBuf>,
        allow_protected: &[String],
        case_insensitive: bool,
    ) -> Result<Self, ToolError> {
        let root = root.canonicalize().map_err(|e| {
            ToolError::new(
                codes::FS_NOTFOUND,
                format!("workspace root {} is not usable: {e}", root.display()),
            )
        })?;
        let grants = additional
            .iter()
            .filter_map(|dir| dir.canonicalize().ok().map(|path| Grant { path, writable }))
            .collect();
        let home = home.and_then(|h| h.canonicalize().ok().or(Some(h)));

        let mut protected = Vec::new();
        let mut add = |pattern: &str, writes_only: bool| {
            protected.push(Protected {
                matcher: glob(pattern, case_insensitive),
                writes_only,
            });
        };
        // §9.4, workspace-relative or anywhere on disk: matched against the
        // canonical absolute path, so `**/` covers nested checkouts too.
        add("**/.git/**", true);
        for pattern in [
            "**/.env",
            "**/.env.*",
            "**/*.pem",
            "**/*.key",
            "**/id_rsa*",
            "**/id_ed25519*",
            "**/credentials",
            "**/secrets.*",
            "**/.netrc",
            "**/.npmrc",
            "**/.pypirc",
        ] {
            add(pattern, false);
        }
        // REQ-SAFE-004: a model must not be able to rewrite the rules that
        // bind it. Readable (so it can explain a denial), never writable.
        add("**/.cairn/permissions.json", true);
        // OS integrity.
        for pattern in [
            "/etc/**",
            "/boot/**",
            "/dev/sda*",
            "/proc/self/mem",
            "C:/Windows/**",
        ] {
            add(pattern, false);
        }
        if let Some(home) = &home {
            let h = to_posix(home);
            for tail in [
                ".ssh/**",
                ".gnupg/**",
                ".aws/**",
                ".config/gh/**",
                ".docker/config.json",
                ".bashrc",
                ".zshrc",
                ".profile",
                ".bash_profile",
                ".config/fish/**",
                ".vimrc",
                ".gitconfig",
                ".config/git/config",
                ".cairn/**",
            ] {
                add(&format!("{h}/{tail}"), false);
            }
            add(&format!("{h}/.config/cairn/permissions.json"), true);
        }
        let unprotected = allow_protected
            .iter()
            .map(|pattern| glob(pattern.trim_start_matches('/'), case_insensitive))
            .collect();
        Ok(Self {
            root,
            grants,
            home,
            protected,
            unprotected,
        })
    }

    /// The canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The effective grants as a message wants them: `workspace, ~/api`.
    fn grant_list(&self) -> String {
        std::iter::once("workspace".to_string())
            .chain(
                self.grants
                    .iter()
                    .map(|g| abbreviate(&g.path, self.home.as_deref())),
            )
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Turn the model's spelling into an absolute, lexically clean path.
    fn lexical(&self, input: &str, base: &Path) -> Result<PathBuf, ToolError> {
        if input.is_empty() || input.contains('\0') {
            return Err(ToolError::new(
                codes::FS_BADPATH,
                "the path is empty or contains a NUL byte",
            )
            .recovery("Pass a workspace-relative path such as `src/lib.rs`."));
        }
        // §6.5 step 3: one separator, whatever the platform the model thinks
        // it is on.
        let mut text = input.replace('\\', "/");
        if text == "~" || text.starts_with("~/") {
            let Some(home) = &self.home else {
                return Err(ToolError::new(
                    codes::FS_BADPATH,
                    "`~` was used but there is no home directory",
                )
                .recovery("Use a workspace-relative path."));
            };
            text = format!("{}{}", to_posix(home), &text[1..]);
        }
        let candidate = PathBuf::from(&text);
        let joined = if candidate.is_absolute() || text.starts_with('/') {
            candidate
        } else {
            base.join(candidate)
        };
        let mut out = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                    out.push(component.as_os_str());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    // `/..` is `/`: popping the root is a no-op, not an error.
                    out.pop();
                }
            }
        }
        Ok(out)
    }

    /// Canonicalise through the deepest ancestor that exists, so a file that
    /// is about to be created is still judged by where its directory really
    /// is.
    fn canonical(path: &Path) -> PathBuf {
        if let Ok(full) = path.canonicalize() {
            return full;
        }
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut probe = path.to_path_buf();
        loop {
            if let Ok(real) = probe.canonicalize() {
                let mut out = real;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            match (
                probe.file_name().map(std::ffi::OsString::from),
                probe.parent(),
            ) {
                (Some(name), Some(parent)) => {
                    tail.push(name);
                    probe = parent.to_path_buf();
                }
                // Nothing on the path exists (not even the root, on an odd
                // platform): the lexical form is the best there is.
                _ => return path.to_path_buf(),
            }
        }
    }

    fn is_protected(&self, abs: &Path, access: Access) -> bool {
        let posix = to_posix(abs);
        if self.unprotected.iter().any(|g| {
            g.is_match(posix.trim_start_matches('/'))
                || self
                    .relative_to_root(abs)
                    .is_some_and(|rel| g.is_match(rel.as_str()))
        }) {
            return false;
        }
        self.protected
            .iter()
            .any(|p| p.matcher.is_match(&posix) && (!p.writes_only || access == Access::Write))
    }

    /// A read-protected path, ignoring `allow_protected_paths`.
    fn is_secret(&self, abs: &Path) -> bool {
        let posix = to_posix(abs);
        self.protected
            .iter()
            .any(|p| !p.writes_only && p.matcher.is_match(&posix))
    }

    fn relative_to_root(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root).ok().map(|rel| {
            let text = to_posix(rel);
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        })
    }

    /// The home directory, if there is one.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Where `input` lands and whether it is protected; `None` for a spelling
    /// that is not a path at all.
    #[must_use]
    pub fn probe(&self, input: &str, base: &Path, access: Access) -> Option<Probe> {
        let lexical = self.lexical(input, base).ok()?;
        let abs = Self::canonical(&lexical);
        let inside =
            abs.starts_with(&self.root) || self.grants.iter().any(|g| abs.starts_with(&g.path));
        let protected = self.is_protected(&abs, access);
        Some(Probe {
            abs,
            inside,
            protected,
        })
    }

    /// Resolve `input` for `access`, relative to `base` (the call's cwd).
    ///
    /// # Errors
    /// `E-FS-BADPATH` for an unusable spelling, `E-FS-ESCAPE` when the
    /// resolved path is outside the workspace and every grant (or inside a
    /// read-only grant and the access is a write).
    pub fn resolve(&self, input: &str, base: &Path, access: Access) -> Result<Resolved, ToolError> {
        let lexical = self.lexical(input, base)?;
        let abs = Self::canonical(&lexical);

        let in_root = abs.starts_with(&self.root);
        let grant = self.grants.iter().find(|g| abs.starts_with(&g.path));
        let allowed = in_root || grant.is_some();
        let writable = in_root || grant.is_some_and(|g| g.writable);
        if !allowed || (access == Access::Write && !writable) {
            let why = if allowed {
                "that grant is read-only"
            } else {
                "outside the workspace"
            };
            return Err(ToolError::new(
                codes::FS_ESCAPE,
                format!(
                    "Path resolves {why} (→ {}). Grants: {}.",
                    to_posix(&abs),
                    self.grant_list()
                ),
            )
            .recovery(
                "Use a path inside the workspace; ask the user to grant additional directories.",
            ));
        }
        Ok(Resolved {
            rel: self.relative_to_root(&abs),
            protected: self.is_protected(&abs, access),
            secret: self.is_secret(&abs),
            abs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(dir: &Path) -> Boundary {
        Boundary::new(dir, &[], false, Some(dir.join("home")), &[], false).expect("boundary")
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tmp");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).expect("dirs");
        std::fs::create_dir_all(tmp.path().join("home")).expect("home");
        (tmp, ws)
    }

    #[test]
    fn relative_paths_resolve_inside_the_workspace() {
        let (tmp, ws) = workspace();
        let b = Boundary::new(&ws, &[], false, Some(tmp.path().join("home")), &[], false)
            .expect("boundary");
        let r = b
            .resolve("src/a.rs", b.root(), Access::Read)
            .expect("resolves");
        assert_eq!(r.rel.as_deref(), Some("src/a.rs"));
        assert_eq!(r.abs, b.root().join("src").join("a.rs"));
        assert!(!r.protected);
        // `.` and the root itself.
        assert_eq!(
            b.resolve(".", b.root(), Access::Read)
                .expect("root")
                .rel
                .as_deref(),
            Some(".")
        );
    }

    #[test]
    fn dotdot_is_resolved_lexically_and_cannot_climb_out() {
        let (tmp, ws) = workspace();
        let b = Boundary::new(&ws, &[], false, Some(tmp.path().join("home")), &[], false)
            .expect("boundary");
        assert_eq!(
            b.resolve("src/../src/./a.rs", b.root(), Access::Read)
                .expect("ok")
                .rel
                .as_deref(),
            Some("src/a.rs")
        );
        let err = b
            .resolve("../ws-evil/x", b.root(), Access::Read)
            .expect_err("escapes");
        assert_eq!(err.code, "E-FS-ESCAPE");
        let err = b
            .resolve("src/../../../../etc/passwd", b.root(), Access::Read)
            .expect_err("escapes");
        assert_eq!(err.code, "E-FS-ESCAPE");
        assert!(err.message.contains("Grants: workspace"), "{}", err.message);
        assert!(err.message.contains("/etc/passwd"), "{}", err.message);
    }

    #[test]
    fn absolute_paths_inside_are_fine_and_outside_are_not() {
        let (_tmp, ws) = workspace();
        let b = boundary(&ws);
        let inside = b.root().join("src/a.rs");
        assert!(b
            .resolve(inside.to_str().expect("utf8"), b.root(), Access::Read)
            .is_ok());
        assert_eq!(
            b.resolve("/etc/hostname", b.root(), Access::Read)
                .expect_err("outside")
                .code,
            "E-FS-ESCAPE"
        );
    }

    #[test]
    fn backslashes_are_separators_and_junk_is_rejected() {
        let (_tmp, ws) = workspace();
        let b = boundary(&ws);
        assert_eq!(
            b.resolve("src\\a.rs", b.root(), Access::Read)
                .expect("ok")
                .rel
                .as_deref(),
            Some("src/a.rs")
        );
        for bad in ["", "a\0b"] {
            assert_eq!(
                b.resolve(bad, b.root(), Access::Read)
                    .expect_err("bad")
                    .code,
                "E-FS-BADPATH"
            );
        }
    }

    /// REQ-CTX-003 / §9.4: a symlink inside the workspace that leads outside
    /// is judged by its destination.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_an_escape() {
        let (tmp, ws) = workspace();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("secret.txt"), "x").expect("file");
        std::os::unix::fs::symlink(&outside, ws.join("link")).expect("symlink");
        let b = Boundary::new(&ws, &[], false, Some(tmp.path().join("home")), &[], false)
            .expect("boundary");
        // Existing target through the link.
        assert_eq!(
            b.resolve("link/secret.txt", b.root(), Access::Read)
                .expect_err("escape")
                .code,
            "E-FS-ESCAPE"
        );
        // A file that does not exist yet, under the link: still judged by
        // where the directory really is.
        assert_eq!(
            b.resolve("link/new.txt", b.root(), Access::Write)
                .expect_err("escape")
                .code,
            "E-FS-ESCAPE"
        );
        // A symlink that stays inside is fine.
        std::os::unix::fs::symlink(ws.join("src"), ws.join("alias")).expect("symlink");
        let r = b
            .resolve("alias/a.rs", b.root(), Access::Read)
            .expect("inside");
        assert_eq!(r.rel.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn grants_are_read_only_unless_declared_writable() {
        let (tmp, ws) = workspace();
        let api = tmp.path().join("api");
        std::fs::create_dir_all(&api).expect("api");
        let ro = Boundary::new(
            &ws,
            std::slice::from_ref(&api),
            false,
            Some(tmp.path().join("home")),
            &[],
            false,
        )
        .expect("boundary");
        let path = api.join("x.txt");
        let p = path.to_str().expect("utf8");
        let read = ro.resolve(p, ro.root(), Access::Read).expect("readable");
        assert_eq!(read.rel, None, "outside the root, so no relative spelling");
        let err = ro
            .resolve(p, ro.root(), Access::Write)
            .expect_err("read-only grant");
        assert_eq!(err.code, "E-FS-ESCAPE");
        assert!(err.message.contains("read-only"), "{}", err.message);

        let rw = Boundary::new(&ws, &[api], true, Some(tmp.path().join("home")), &[], false)
            .expect("boundary");
        assert!(rw.resolve(p, rw.root(), Access::Write).is_ok());
    }

    #[test]
    fn secrets_are_protected_for_reads_and_writes() {
        let (_tmp, ws) = workspace();
        let b = boundary(&ws);
        for name in [
            ".env",
            ".env.production",
            "config/server.pem",
            "deploy/id_rsa",
            "deploy/id_ed25519.pub",
            "a/credentials",
            "secrets.yaml",
            ".netrc",
            ".npmrc",
            ".pypirc",
            "tls/private.key",
        ] {
            for access in [Access::Read, Access::Write] {
                let r = b
                    .resolve(name, b.root(), access)
                    .expect("inside the workspace");
                assert!(r.protected, "{name} {access:?}");
            }
        }
        assert!(
            !b.resolve("src/environment.rs", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
        assert!(
            !b.resolve("src/keyboard.rs", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
    }

    /// `.git` is readable (the git tools need it) and unwritable; Cairn's own
    /// permission file is the same.
    #[test]
    fn git_and_the_permission_file_are_write_protected_only() {
        let (_tmp, ws) = workspace();
        let b = boundary(&ws);
        for name in [
            ".git/config",
            ".git/hooks/pre-commit",
            "sub/.git/HEAD",
            ".cairn/permissions.json",
        ] {
            assert!(
                !b.resolve(name, b.root(), Access::Read)
                    .expect("ok")
                    .protected,
                "read {name}"
            );
            assert!(
                b.resolve(name, b.root(), Access::Write)
                    .expect("ok")
                    .protected,
                "write {name}"
            );
        }
        assert!(
            !b.resolve(".cairn/todos.json", b.root(), Access::Write)
                .expect("ok")
                .protected
        );
        assert!(
            !b.resolve(".gitignore", b.root(), Access::Write)
                .expect("ok")
                .protected
        );
    }

    #[test]
    fn home_credentials_and_shell_profiles_are_protected() {
        let (tmp, ws) = workspace();
        let home = tmp.path().join("home");
        let b = Boundary::new(
            &ws,
            std::slice::from_ref(&home),
            true,
            Some(home.clone()),
            &[],
            false,
        )
        .expect("boundary");
        for name in [
            ".ssh/id_x",
            ".gnupg/key",
            ".aws/credentials",
            ".config/gh/hosts.yml",
            ".bashrc",
            ".zshrc",
            ".gitconfig",
            ".config/git/config",
            ".cairn/anything",
            ".config/fish/config.fish",
        ] {
            let p = home.join(name);
            let r = b
                .resolve(p.to_str().expect("utf8"), b.root(), Access::Read)
                .expect("granted");
            assert!(r.protected, "{name}");
        }
        let notes = home.join("notes.txt");
        assert!(
            !b.resolve(notes.to_str().expect("utf8"), b.root(), Access::Read)
                .expect("ok")
                .protected
        );
    }

    #[test]
    fn allow_protected_paths_lifts_one_protection() {
        let (tmp, ws) = workspace();
        let b = Boundary::new(
            &ws,
            &[],
            false,
            Some(tmp.path().join("home")),
            &["config/*.pem".to_string()],
            false,
        )
        .expect("boundary");
        assert!(
            !b.resolve("config/tls.pem", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
        assert!(
            b.resolve("other/tls.pem", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
        assert!(
            b.resolve(".env", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
    }

    #[test]
    fn case_insensitive_volumes_match_protection_in_any_case() {
        let (tmp, ws) = workspace();
        let b = Boundary::new(&ws, &[], false, Some(tmp.path().join("home")), &[], true)
            .expect("boundary");
        assert!(
            b.resolve(".ENV", b.root(), Access::Read)
                .expect("ok")
                .protected
        );
        let sensitive = boundary(&ws);
        assert!(
            !sensitive
                .resolve(".ENV", sensitive.root(), Access::Read)
                .expect("ok")
                .protected
        );
    }

    #[test]
    fn a_missing_workspace_root_is_an_error_not_a_panic() {
        let err = Boundary::new(
            Path::new("/definitely/not/here"),
            &[],
            false,
            None,
            &[],
            false,
        )
        .expect_err("no root");
        assert_eq!(err.code, "E-FS-NOTFOUND");
    }

    #[test]
    fn tilde_needs_a_home() {
        let (_tmp, ws) = workspace();
        let b = Boundary::new(&ws, &[], false, None, &[], false).expect("boundary");
        assert_eq!(
            b.resolve("~/x", b.root(), Access::Read)
                .expect_err("no home")
                .code,
            "E-FS-BADPATH"
        );
    }
}
