//! identifies a VMM process across agent restarts, so a leftover lease can be
//! checked or cleaned up without trusting a bare PID that might've been reused

use std::{
    fmt, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::OnceLock,
    time::Duration,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use stable_eyre::{Result, eyre::eyre};

/// the kernel's random ID for the current boot. start times count from boot,
/// so a PID plus start time only means something within the boot it came from
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BootId(u128);

impl BootId {
    pub fn current() -> io::Result<Self> {
        static CURRENT: OnceLock<BootId> = OnceLock::new();
        if let Some(boot_id) = CURRENT.get() {
            return Ok(*boot_id);
        }
        let contents = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let boot_id = contents
            .trim()
            .parse()
            .map_err(|()| io::Error::new(io::ErrorKind::InvalidData, "malformed kernel boot_id"))?;
        Ok(*CURRENT.get_or_init(|| boot_id))
    }
}

impl std::str::FromStr for BootId {
    type Err = ();

    /// the kernel's UUID form, like `a15ea56c-ab53-44ab-b5b0-9f498b0e38a2`
    fn from_str(value: &str) -> Result<Self, ()> {
        let hex: String = value.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 || value.len() != 36 {
            return Err(());
        }
        u128::from_str_radix(&hex, 16).map(Self).map_err(drop)
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = format!("{:032x}", self.0);
        write!(
            f,
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    }
}

impl fmt::Debug for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for BootId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BootId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value
            .parse()
            .map_err(|()| serde::de::Error::custom(format!("invalid boot ID {value:?}")))
    }
}

/// a PID plus its kernel start time and the boot it started in. PIDs get
/// reused and start times restart at every boot, but all three together can't
/// point at a different process
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
    pub boot_id: BootId,
}

struct ProcessStat {
    state: char,
    start_time: u64,
}

impl ProcessStat {
    /// zombies have already let go of their fds and vsock CID
    const fn is_dead(&self) -> bool {
        matches!(self.state, 'Z' | 'X')
    }
}

fn read_stat(pid: u32) -> io::Result<Option<ProcessStat>> {
    read_stat_at(&format!("/proc/{pid}/stat"))
}

/// whether any thread in the process is still alive. the leader can exit on
/// its own and show up as a zombie while other threads keep going, like CH's
/// disk threads stuck in D state on a hung device
fn has_live_thread(pid: u32) -> io::Result<bool> {
    let tasks = match std::fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(tasks) => tasks,
        Err(error) if is_gone(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    for task in tasks {
        let task = match task {
            Ok(task) => task,
            Err(error) if is_gone(&error) => continue,
            Err(error) => return Err(error),
        };
        let path = task.path().join("stat");
        if read_stat_at(&path.to_string_lossy())?.is_some_and(|stat| !stat.is_dead()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn read_stat_at(path: &str) -> io::Result<Option<ProcessStat>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if is_gone(&error) => return Ok(None),
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
            boot_id: BootId::current()?,
        })
    }

    /// a process from an earlier boot is gone, whatever has its PID now. a
    /// zombie only counts as exited once every one of its threads is dead
    pub fn is_running(self) -> io::Result<bool> {
        if self.boot_id != BootId::current()? {
            return Ok(false);
        }
        let Some(stat) = read_stat(self.pid)? else {
            return Ok(false);
        };
        if stat.start_time != self.start_time {
            return Ok(false);
        }
        if !stat.is_dead() {
            return Ok(true);
        }
        // only trust the thread list if the PID still belongs to the same
        // process after we read it
        let live_thread = has_live_thread(self.pid)?;
        Ok(live_thread
            && read_stat(self.pid)?.is_some_and(|stat| stat.start_time == self.start_time))
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
    use super::{BootId, ProcessIdentity};
    use std::time::Duration;

    #[test]
    fn boot_id_round_trips_through_the_kernel_format() {
        let raw = "a15ea56c-ab53-44ab-b5b0-9f498b0e38a2";
        let boot_id: BootId = raw.parse().unwrap();
        assert_eq!(boot_id.to_string(), raw);
        assert_eq!(
            serde_json::to_string(&boot_id).unwrap(),
            format!("\"{raw}\"")
        );
        "a15ea56cab5344abb5b09f498b0e38a2"
            .parse::<BootId>()
            .unwrap_err();
        "not-a-boot-id".parse::<BootId>().unwrap_err();
        BootId::current().expect("read this boot's ID");
    }

    #[tokio::test]
    async fn identity_from_another_boot_is_never_running_or_signalled() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let identity = ProcessIdentity::of(child.id().expect("child pid")).expect("read identity");
        // same PID and start time, recorded before a reboot
        let other_boot = ProcessIdentity {
            boot_id: BootId(identity.boot_id.0 ^ 1),
            ..identity
        };
        assert!(!other_boot.is_running().expect("check old-boot identity"));
        other_boot
            .terminate(Duration::from_secs(1))
            .await
            .expect("old-boot terminate is a no-op");
        assert!(identity.is_running().expect("still running"));
        child.kill().await.expect("kill sleep");
    }

    #[tokio::test]
    async fn zombie_leader_with_a_live_thread_still_counts_as_running() {
        // the main thread exits on its own (SYS_exit, not exit_group) while a
        // second thread sleeps. python is the easy way to get that shape
        #[cfg(target_arch = "x86_64")]
        const SYS_EXIT: u32 = 60;
        #[cfg(target_arch = "aarch64")]
        const SYS_EXIT: u32 = 93;
        let script = format!(
            "import threading, time, ctypes\n\
             threading.Thread(target=time.sleep, args=(30,)).start()\n\
             ctypes.CDLL(None).syscall({SYS_EXIT}, 0)\n"
        );
        let Ok(mut child) = tokio::process::Command::new("python3")
            .args(["-c", &script])
            .kill_on_drop(true)
            .spawn()
        else {
            eprintln!("python3 isn't installed, skipping");
            return;
        };
        let identity = ProcessIdentity::of(child.id().expect("child pid")).expect("read identity");
        let pid = identity.pid;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !super::read_stat(pid)
            .expect("read stat")
            .is_some_and(|stat| stat.is_dead())
        {
            assert!(std::time::Instant::now() < deadline, "leader never exited");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(identity.is_running().expect("check zombie leader"));
        identity
            .terminate(Duration::from_secs(5))
            .await
            .expect("terminate kills the remaining thread");
        assert!(!identity.is_running().expect("whole group is dead"));
        drop(child.wait().await);
    }

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
