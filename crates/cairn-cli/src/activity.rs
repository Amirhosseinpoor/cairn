//! "Is a turn in flight?" — the question `cairn update` must answer before it
//! replaces the binary (SPEC §1.5, REQ-PROD-003, T-OPS-002, T-CLI-016).
//!
//! Several `cairn run`s may overlap, so there is no single lock: each turn
//! drops a marker `active-turns/<pid>` for its lifetime, and a marker only
//! counts while its process is alive. A crash leaves a stale marker, which
//! the next check removes. Nothing here is a security boundary — it stops a
//! person from replacing a binary under their own running agent.

use std::path::{Path, PathBuf};

const DIR: &str = "active-turns";

/// Held for the life of one turn; the marker disappears when it drops.
#[derive(Debug)]
pub struct TurnGuard {
    marker: PathBuf,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// Record that this process is running a turn. Best effort: a cache directory
/// that cannot be written must not stop the turn, so failure is `None`.
#[must_use]
pub fn begin(cache_home: &Path) -> Option<TurnGuard> {
    begin_as(cache_home, std::process::id())
}

fn begin_as(cache_home: &Path, pid: u32) -> Option<TurnGuard> {
    let dir = cache_home.join(DIR);
    std::fs::create_dir_all(&dir).ok()?;
    let marker = dir.join(pid.to_string());
    std::fs::write(&marker, pid.to_string()).ok()?;
    Some(TurnGuard { marker })
}

/// Pids of turns that are running now. Markers whose process is gone are
/// deleted on the way.
#[must_use]
pub fn active(cache_home: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(cache_home.join(DIR)) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid_alive(pid) {
            live.push(pid);
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    live.sort_unstable();
    live
}

/// Whether process `pid` exists. `/proc` where there is one; otherwise ask
/// the platform's own tool, because probing with a signal needs `unsafe`.
fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    if Path::new("/proc/self").exists() {
        return Path::new("/proc").join(pid.to_string()).exists();
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_is_active_only_while_its_guard_lives() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(active(dir.path()).is_empty());
        let guard = begin(dir.path()).expect("marker written");
        assert_eq!(active(dir.path()), vec![std::process::id()]);
        drop(guard);
        assert!(active(dir.path()).is_empty());
    }

    /// A crashed turn leaves a marker for a pid nobody owns; checking
    /// removes it instead of blocking updates forever.
    #[test]
    fn a_stale_marker_is_pruned_not_counted() {
        let dir = tempfile::tempdir().expect("tmp");
        let ghost = u32::MAX - 7;
        std::mem::forget(begin_as(dir.path(), ghost).expect("marker"));
        assert!(active(dir.path()).is_empty());
        assert!(!dir.path().join(DIR).join(ghost.to_string()).exists());
    }

    #[test]
    fn junk_in_the_directory_is_ignored() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(dir.path().join(DIR)).expect("dir");
        std::fs::write(dir.path().join(DIR).join("not-a-pid"), "x").expect("junk");
        assert!(active(dir.path()).is_empty());
    }

    #[test]
    fn an_unwritable_cache_does_not_stop_a_turn() {
        let dir = tempfile::tempdir().expect("tmp");
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").expect("file");
        assert!(
            begin(&file).is_none(),
            "cache_home is a file, not a directory"
        );
    }
}
