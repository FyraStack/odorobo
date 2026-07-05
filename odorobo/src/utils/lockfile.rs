use tokio::io::AsyncWriteExt;

const LOCKFILE: &str = "/var/lock/odorobo.lock";

// `Option` needed since `std::mem::take` requires `impl Default`
pub struct Lockfile(Option<tokio::fs::File>);

impl Drop for Lockfile {
    fn drop(&mut self) {
        std::mem::drop(std::mem::take(&mut self.0).expect("None in Lockfile"));
        _ = std::fs::remove_file(LOCKFILE)
            .inspect_err(|e| tracing::warn!(?LOCKFILE, ?e, "cannot remove lockfile"));
    }
}

/// Create an odorobo lockfile
///
/// See #30, only 1 instance of odorobo should run.
pub async fn init_lockfile() -> Result<Lockfile, String> {
    let mut f = tokio::fs::File::create_new(LOCKFILE)
        .await
        .map_err(|e| format!("Cannot create lockfile at {LOCKFILE}: {e:?}"))?;
    f.write_all(&std::process::id().to_ne_bytes())
        .await
        .map_err(|e| format!("cannot write to {LOCKFILE}: {e:?}"))?;
    Ok(Lockfile(Some(f)))
}
