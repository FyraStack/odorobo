use std::io::Write;

const LOCKFILE: &str = "/var/lock/odorobo.lock";

// `Option` needed since `std::mem::take` requires `impl Default`
pub struct Lockfile(Option<std::fs::File>);

impl Drop for Lockfile {
    fn drop(&mut self) {
        tracing::debug!("dropping Lockfile");
        std::mem::drop(std::mem::take(&mut self.0).expect("None in Lockfile"));
        _ = std::fs::remove_file(LOCKFILE)
            .inspect_err(|e| tracing::warn!(?LOCKFILE, ?e, "cannot remove lockfile"));
    }
}

/// Create an odorobo lockfile
///
/// See #30, only 1 instance of odorobo should run.
pub fn init_lockfile() -> Result<Lockfile, String> {
    tracing::trace!("creating lockfile at {LOCKFILE}");
    let mut f = std::fs::File::create_new(LOCKFILE)
        .map_err(|e| format!("Cannot create lockfile at {LOCKFILE}: {e:?}"))?;
    f.write_all(&std::process::id().to_ne_bytes())
        .map_err(|e| format!("cannot write to {LOCKFILE}: {e:?}"))?;
    f.flush()
        .map_err(|e| format!("cannot flush {LOCKFILE}: {e:?}"))?;
    Ok(Lockfile(Some(f)))
}

/// Register termination signals to watch
///
/// We need to tidy up lockfiles before exiting. Watching these signals can give us a chance to
/// actually clean things up.
pub fn register_termsigs() -> std::io::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    let term = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in signal_hook::consts::TERM_SIGNALS {
        signal_hook::flag::register_conditional_shutdown(*sig, 1, std::sync::Arc::clone(&term))?;
        signal_hook::flag::register(*sig, std::sync::Arc::clone(&term))?;
    }
    Ok(term)
}
