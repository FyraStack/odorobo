//! identifies a VMM process across agent restarts, so a leftover lease can be
//! checked or cleaned up without trusting a bare PID that might've been reused

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use stable_eyre::{Result, eyre::eyre};

/// a PID plus its kernel start time. the pair can't get reused, unlike the PID alone
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

struct ProcessStat {
    state: char,
    start_time: u64,
}

fn read_stat(pid: u32) -> io::Result<Option<ProcessStat>> {
    let contents = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
        Err(error) => return Err(error),
    };
    // comm can have spaces and parens in it, so skip to the last ')'
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat");
    let (_, fields) = contents.rsplit_once(')').ok_or_else(invalid)?;
    let mut fields = fields.split_whitespace();
    let state = fields
        .next()
        .and_then(|state| state.chars().next())
        .ok_or_else(invalid)?;
    // starttime is field 22, and state was field 3
    let start_time = fields
        .nth(18)
        .and_then(|start_time| start_time.parse().ok())
        .ok_or_else(invalid)?;
    Ok(Some(ProcessStat { state, start_time }))
}

impl ProcessIdentity {
    pub fn of(pid: u32) -> io::Result<Self> {
        let stat = read_stat(pid)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "process already exited"))?;
        Ok(Self {
            pid,
            start_time: stat.start_time,
        })
    }

    /// zombies count as exited, they've already let go of their fds and vsock CID
    pub fn is_running(self) -> io::Result<bool> {
        Ok(read_stat(self.pid)?.is_some_and(|stat| {
            stat.start_time == self.start_time && !matches!(stat.state, 'Z' | 'X')
        }))
    }

    /// SIGKILL this exact process (if it's still around) and wait for it to die
    pub async fn terminate(self, timeout: Duration) -> Result<()> {
        let Some(pidfd) = pidfd_open(self.pid)
            .map_err(|error| eyre!("failed to open pidfd for VMM {}: {error}", self.pid))?
        else {
            return Ok(());
        };
        // the pidfd sticks to whichever process had the PID when we opened it.
        // so if the identity still checks out now, the signal can't hit a different one
        if !self.is_running()? {
            return Ok(());
        }
        pidfd_kill(&pidfd).map_err(|error| eyre!("failed to kill VMM {}: {error}", self.pid))?;

        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| eyre!("VMM kill timeout overflowed"))?;
        while self.is_running()? {
            if tokio::time::Instant::now() >= deadline {
                return Err(eyre!("VMM {} didn't exit after SIGKILL", self.pid));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(pidfd);
        Ok(())
    }
}

/// returns `None` if the process is already gone
fn pidfd_open(pid: u32) -> io::Result<Option<OwnedFd>> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID is out of range"))?;
    // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    let raw = i32::try_from(raw).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pidfd_open returned an invalid fd",
        )
    })?;
    // SAFETY: pidfd_open just handed us this fd and nothing else owns it
    Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
}

/// a process that already exited isn't an error
fn pidfd_kill(pidfd: &OwnedFd) -> io::Result<()> {
    // SAFETY: the fd is a live pidfd, and a null siginfo is allowed
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProcessIdentity;
    use std::time::Duration;

    #[tokio::test]
    async fn identity_tracks_and_kills_the_exact_process() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let identity = ProcessIdentity::of(child.id().expect("child pid")).expect("read identity");
        assert!(identity.is_running().expect("check identity"));

        let stale = ProcessIdentity {
            start_time: identity.start_time.wrapping_add(1),
            ..identity
        };
        assert!(!stale.is_running().expect("check stale identity"));
        // a stale identity never gets signalled, even though the PID is alive
        stale
            .terminate(Duration::from_secs(1))
            .await
            .expect("stale terminate is a no-op");
        assert!(identity.is_running().expect("check identity again"));

        identity
            .terminate(Duration::from_secs(5))
            .await
            .expect("terminate sleep");
        assert!(!identity.is_running().expect("zombie counts as exited"));
        drop(child.wait().await);
    }
}
