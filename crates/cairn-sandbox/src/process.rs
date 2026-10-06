//! Signalling the process group of a child (SPEC §6.4.6).
//!
//! A command run by `bash` is a process-group leader, so a timeout or a
//! cancel can stop everything it started, not only the shell. Sending a
//! signal to a group is a system call with no safe wrapper in `std`; it lives
//! here because this crate is the one place `unsafe` is allowed.

/// A signal Cairn sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Int,
    Kill,
}

impl Signal {
    /// The name the model uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Kill => "SIGKILL",
        }
    }
}

/// Send `signal` to every process in the group led by `pid`.
///
/// On Windows there are no signals: the tree is ended with `taskkill /T /F`
/// whatever the signal.
///
/// # Errors
/// The OS error, for example when the group is already gone.
#[cfg(unix)]
pub fn signal_group(pid: u32, signal: Signal) -> std::io::Result<()> {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Int => libc::SIGINT,
        Signal::Kill => libc::SIGKILL,
    };
    let Ok(pid) = i32::try_from(pid) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    if pid <= 1 {
        // Never `kill(-1)` or `kill(0)`: those reach far beyond a child.
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: `kill` is called with a negated pid greater than 1, which names
    // one process group, and a signal number from the fixed set above; it
    // reads and writes no memory of ours.
    let rc = unsafe { libc::kill(-pid, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Send `signal` to every process in the tree rooted at `pid`.
///
/// # Errors
/// The error from running `taskkill`.
#[cfg(windows)]
pub fn signal_group(pid: u32, _signal: Signal) -> std::io::Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_whole_process_group_is_stopped() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30; wait"])
            .process_group(0)
            .spawn()
            .expect("spawns");
        std::thread::sleep(std::time::Duration::from_millis(100));
        signal_group(child.id(), Signal::Term).expect("signals");
        let status = child.wait().expect("waits");
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn pids_that_would_reach_everything_are_refused() {
        for pid in [0, 1] {
            assert!(signal_group(pid, Signal::Kill).is_err());
        }
    }

    #[test]
    fn signals_have_the_names_the_model_uses() {
        assert_eq!(Signal::Term.name(), "SIGTERM");
        assert_eq!(Signal::Kill.name(), "SIGKILL");
    }
}
