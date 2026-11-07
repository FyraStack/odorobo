//! PID 1 for Odorobo microVMs.
//!
//! The kernel boots this binary from a small initramfs built by this crate's
//! justfile. It mounts the device, proc and sys filesystems, mounts the image
//! root over the virtiofs `rootfs` tag, applies the OCI image configuration
//! the host placed in that root as `/.odorobo-config.json`, and runs the
//! workload. It then stays around as PID 1 to reap orphans, forward shutdown
//! signals and power the VM off when the workload exits.

mod image;
mod init;

use std::process::ExitCode;

fn main() -> ExitCode {
    init::run()
}
