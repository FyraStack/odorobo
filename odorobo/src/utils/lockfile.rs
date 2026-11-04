use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use fs2::FileExt;

const LOCKFILE: &str = "/var/lock/odorobo.lock";

/// holds an exclusive flock on the lockfile for as long as it's alive.
///
/// the kernel drops the lock when the process dies, however it dies (SIGKILL
/// from systemd's stop timeout, a second signal's `exit(1)`, a panic with
/// abort), so a leftover file never blocks the next start. the file itself is
/// never removed. unlinking it while another process has it open would let two
/// instances each lock a different inode
pub struct Lockfile(Option<File>);

impl Drop for Lockfile {
    fn drop(&mut self) {
        if self.0.take().is_some() {
            // closing the fd releases the flock
            tracing::debug!("released lockfile");
        }
    }
}

/// Create an odorobo lockfile
///
/// See #30, only 1 instance of odorobo should run.
pub fn init_lockfile(cfg: &crate::config::Config) -> Result<Lockfile, String> {
    if cfg.no_lockfile.unwrap_or_default() {
        tracing::info!("Skipping lockfile due to `no_lockfile`");
        return Ok(Lockfile(None));
    }
    acquire(Path::new(LOCKFILE))
}

fn acquire(path: &Path) -> Result<Lockfile, String> {
    tracing::trace!(?path, "taking lockfile");
    // std opens with O_CLOEXEC, so spawned VMMs don't inherit the fd and keep
    // the lock alive after we exit
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("Cannot open lockfile at {}: {e:?}", path.display()))?;
    if let Err(e) = f.try_lock_exclusive() {
        let holder = std::fs::read_to_string(path).unwrap_or_default();
        let holder = holder.trim();
        return Err(if holder.is_empty() {
            format!(
                "{} is locked by another odorobo instance: {e}",
                path.display()
            )
        } else {
            format!(
                "{} is locked by another odorobo instance (pid {holder}): {e}",
                path.display()
            )
        });
    }
    // only touch the contents once we hold the lock
    f.set_len(0)
        .and_then(|()| f.seek(SeekFrom::Start(0)).map(drop))
        .and_then(|()| writeln!(f, "{}", std::process::id()))
        .and_then(|()| f.flush())
        .map_err(|e| format!("cannot write to {}: {e:?}", path.display()))?;
    Ok(Lockfile(Some(f)))
}

/// Register termination signals to watch
///
/// the first signal sets the flag so main can stop VMs gracefully. a second one
/// exits right away, which is fine for the lockfile since the kernel releases
/// its flock on exit
pub fn register_termsigs() -> std::io::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in signal_hook::consts::TERM_SIGNALS {
        signal_hook::flag::register_conditional_shutdown(*sig, 1, std::sync::Arc::clone(&term))?;
        signal_hook::flag::register(*sig, std::sync::Arc::clone(&term))?;
    }
    Ok(term)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "odorobo-lockfile-test-{}.lock",
            ulid::Ulid::generate()
        ))
    }

    #[test]
    fn second_instance_is_refused_while_the_lock_is_held() {
        let path = temp_lock_path();

        let held = acquire(&path).unwrap();
        let error = acquire(&path).err().unwrap();
        assert!(
            error.contains("locked by another odorobo instance"),
            "{error}"
        );
        assert!(
            error.contains(&format!("pid {}", std::process::id())),
            "{error}"
        );

        drop(held);
        assert!(
            path.exists(),
            "the lockfile is kept, only the lock is released"
        );
        drop(acquire(&path).unwrap());
        _ = std::fs::remove_file(&path);
    }

    #[test]
    fn leftover_file_from_a_killed_instance_does_not_block_startup() {
        let path = temp_lock_path();
        // what a SIGKILLed instance leaves behind: the file, but no lock
        std::fs::write(&path, "4242\n").unwrap();

        let lock = acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(lock);
        _ = std::fs::remove_file(&path);
    }
}
