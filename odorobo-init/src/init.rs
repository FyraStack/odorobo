//! PID 1 behaviour: mount the image, start the workload, supervise the VM.

use std::convert::Infallible;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::os::fd::RawFd;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::image::{ImageConfig, ImageConfigDocument};
use crate::net;
use tracing::{error, info, warn};

const ROOTFS_TAG: &CStr = c"rootfs";
const ROOT_MOUNT: &str = "/newroot";
/// Guest-private tmpfs holding an ephemeral root's writable overlay layer.
const UPPER_MOUNT: &str = "/upper";
const UPPER_DIR: &str = "/upper/rootfs.upper";
const WORK_DIR: &str = "/upper/rootfs.work";
/// Merged root of an ephemeral VM: the image share plus the tmpfs upper.
const OVERLAY_MOUNT: &str = "/overlay";
/// The host writes the image's OCI config into the rootfs's writable layer.
const CONFIG_FILE: &str = "/newroot/.odorobo-config.json";
const MAX_CONFIG_BYTES: usize = 1 << 20;
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Used when the image has neither an entrypoint nor a command.
const FALLBACK_INIT: [&str; 4] = ["/sbin/init", "/etc/init", "/bin/init", "/bin/sh"];

const HANDLED_SIGNALS: [libc::c_int; 5] = [
    libc::SIGCHLD,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGHUP,
];

/// How long a forwarded termination signal may take before the workload is
/// killed with SIGKILL.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Write end of the self-pipe the signal handler reports through.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

/// Write init logs to the serial console through the current stderr.
fn init_logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .compact()
        .init();
}

/// Runs as PID 1 and never returns on success: the VM is powered off when the
/// workload exits.
pub fn run() -> ExitCode {
    init_logging();
    // SAFETY: getpid takes no arguments and cannot fail.
    let pid = unsafe { libc::getpid() };
    if pid != 1 {
        error!(pid, "must run as PID 1");
        return ExitCode::FAILURE;
    }
    restore_console();
    match run_pid1() {
        Ok(code) => power_off(code),
        Err(err) => {
            error!(error = %err, "fatal");
            power_off(1)
        }
    }
}

/// The initramfs has no console node; the kernel may have booted with closed
/// stdio. Create the node and take over the console before anything can fail.
fn restore_console() {
    fs::create_dir_all("/dev").ok();
    // SAFETY: mknod takes a fixed path and a plain device number.
    let result = unsafe {
        libc::mknod(
            c"/dev/console".as_ptr(),
            libc::S_IFCHR | 0o600,
            libc::makedev(5, 1),
        )
    };
    if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
        return;
    }
    // SAFETY: the node exists at this point, so open returns a console fd.
    let fd = unsafe { libc::open(c"/dev/console".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return;
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: dup2 duplicates fd onto each standard descriptor.
        unsafe { libc::dup2(fd, target) };
    }
    if fd > libc::STDERR_FILENO {
        // SAFETY: fd is no longer needed after the dup2 calls.
        unsafe { libc::close(fd) };
    }
}

fn run_pid1() -> io::Result<i32> {
    mount_early_filesystems()?;
    if kernel_console_is_virtio() {
        // Nothing else will: the virtio console never saw the records the
        // kernel printed before it registered.
        dump_kernel_log();
    }
    let workload = mount_image()?;
    if let Err(err) = net::configure(&workload.network) {
        warn!(%err, "static network configuration failed; continuing without it");
    } else if !workload.network.is_empty() {
        info!(
            interfaces = workload.network.len(),
            "applied static network configuration"
        );
    }
    if let Some(hostname) = &workload.hostname {
        match set_hostname(hostname) {
            Ok(()) => info!(hostname, "hostname set"),
            Err(err) => warn!(%err, hostname, "cannot set the hostname"),
        }
    }
    let read_fd = signal_pipe()?;
    install_signal_handlers()?;

    // The workload runs as PID 1 in its own PID namespace, so images that ship
    // an init (systemd, s6, tini) find the environment they expect. This process
    // stays the global PID 1 and keeps reaping, signaling and powering off.
    // SAFETY: unshare takes plain flags and the process is single-threaded.
    if unsafe { libc::unshare(libc::CLONE_NEWPID) } < 0 {
        return Err(with_context(
            io::Error::last_os_error(),
            "creating the workload PID namespace",
        ));
    }

    // SAFETY: the process is single-threaded here, so fork is safe.
    let child = unsafe { libc::fork() };
    match child {
        0 => child_main(&workload),
        pid if pid > 0 => {
            // A child that takes the workload console becomes a session leader
            // for its own controlling terminal and must not be pre-set as a
            // process group leader; otherwise whichever side wins closes the
            // race here.
            if !workload_uses_virtio_console() {
                // SAFETY: pid is a live child.
                unsafe { libc::setpgid(pid, pid) };
            }
            info!(pid, args = ?workload.process.args(), "workload started");
            let info = supervise(pid, read_fd)?;
            info!(
                description = %info.description,
                status = info.code,
                "workload exited"
            );
            Ok(info.code)
        }
        _ => Err(io::Error::last_os_error()),
    }
}

/// The kernel command line, empty when it cannot be read.
fn kernel_cmdline() -> String {
    fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

/// Whether the kernel's console is the virtio console (`console=hvc0`).
fn kernel_console_is_virtio() -> bool {
    kernel_cmdline()
        .split_ascii_whitespace()
        .any(|argument| argument == "console=hvc0")
}

/// The workload's stdio rides the virtio console whenever the kernel console is
/// not on it: the split layout keeps kernel and init logs on the serial port.
fn workload_uses_virtio_console() -> bool {
    !kernel_console_is_virtio()
}

/// Give the workload the virtio console as its stdout/stderr and, when asked,
/// as its stdin, with a controlling terminal so interactive tools and Ctrl-C
/// work.
fn attach_workload_console(attach_stdin: bool) {
    // SAFETY: opening a fixed device path read-write.
    let fd = unsafe { libc::open(c"/dev/hvc0".as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        warn!(
            error = %io::Error::last_os_error(),
            "cannot open /dev/hvc0; the workload keeps the init console"
        );
        return;
    }
    for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: dup2 duplicates fd onto each standard descriptor.
        unsafe { libc::dup2(fd, target) };
    }
    if attach_stdin {
        // SAFETY: dup2 duplicates fd onto the standard input descriptor.
        unsafe { libc::dup2(fd, libc::STDIN_FILENO) };
    } else {
        // SAFETY: opening a fixed device path read-only.
        let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        if null >= 0 {
            // SAFETY: dup2 duplicates null onto the standard input descriptor.
            unsafe { libc::dup2(null, libc::STDIN_FILENO) };
            if null > libc::STDERR_FILENO {
                // SAFETY: null is no longer needed after the dup2 call.
                unsafe { libc::close(null) };
            }
        } else {
            warn!(
                error = %io::Error::last_os_error(),
                "cannot open /dev/null; closing the workload's stdin"
            );
            // SAFETY: closing an inherited descriptor.
            unsafe { libc::close(libc::STDIN_FILENO) };
        }
    }
    // SAFETY: the child has no controlling terminal yet, so this either takes
    // the fresh terminal or fails because the parent set a process group.
    if unsafe { libc::setsid() } >= 0 {
        // SAFETY: fd is an open tty and the process is now a session leader.
        unsafe { libc::ioctl(fd, libc::TIOCSCTTY, 0) };
    } else {
        warn!(
            error = %io::Error::last_os_error(),
            "workload console cannot become the controlling terminal"
        );
    }
    if fd > libc::STDERR_FILENO {
        // SAFETY: fd is no longer needed after the dup2 calls.
        unsafe { libc::close(fd) };
    }
}

/// The virtio console registers mid-boot and never receives the records the
/// kernel printed before that, so print the ring buffer here: with `quiet` in
/// the cmdline the console then carries the whole boot log in order, ahead of
/// the init's own lines and the workload.
fn dump_kernel_log() {
    use std::io::Write as _;

    const SYSLOG_ACTION_READ_ALL: libc::c_int = 3;
    const SYSLOG_ACTION_SIZE_BUFFER: libc::c_int = 10;
    const MAX_KERNEL_LOG_BYTES: usize = 1 << 20;
    // SAFETY: a size query passes no buffer.
    let size = unsafe { libc::klogctl(SYSLOG_ACTION_SIZE_BUFFER, std::ptr::null_mut(), 0) };
    let Ok(len) = usize::try_from(size) else {
        return;
    };
    let len = len.min(MAX_KERNEL_LOG_BYTES);
    if len == 0 {
        return;
    }
    let mut buffer = vec![0_u8; len];
    // SAFETY: buffer is a writable region of the length given to klogctl.
    let read = unsafe {
        libc::klogctl(
            SYSLOG_ACTION_READ_ALL,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            libc::c_int::try_from(len).unwrap_or(libc::c_int::MAX),
        )
    };
    let Ok(read) = usize::try_from(read) else {
        return;
    };
    // The ring buffer keeps syslog level prefixes; the console does not print
    // them, so drop them here to match live output.
    let mut text = Vec::with_capacity(read);
    for line in buffer[..read].split_inclusive(|byte| *byte == b'\n') {
        text.extend_from_slice(strip_level_prefix(line));
    }

    #[expect(
        clippy::let_underscore_must_use,
        reason = "a missing console must not stop the init"
    )]
    let _: io::Result<()> = std::io::stdout().write_all(&text);
}

fn strip_level_prefix(line: &[u8]) -> &[u8] {
    let Some(rest) = line.strip_prefix(b"<") else {
        return line;
    };
    let Some(end) = rest.iter().position(|byte| *byte == b'>') else {
        return line;
    };
    if end == 0 || end > 3 || !rest[..end].iter().all(u8::is_ascii_digit) {
        return line;
    }
    rest.get(end..)
        .and_then(|tail| tail.get(1..))
        .unwrap_or(line)
}

fn child_main(workload: &Workload) -> ! {
    if workload_uses_virtio_console() {
        attach_workload_console(workload.stdin);
    }
    // SAFETY: pid 0 means the calling process.
    unsafe { libc::setpgid(0, 0) };
    for signal in HANDLED_SIGNALS {
        // SAFETY: restoring the default disposition is always valid.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
    mount_workload_proc();
    match exec_workload(&workload.process) {
        Ok(never) => match never {},
        Err(err) => {
            error!(error = %err, "cannot start workload");
            // SAFETY: _exit terminates the child without running destructors.
            unsafe { libc::_exit(127) }
        }
    }
}

/// The initramfs `/proc` belongs to the outer PID namespace; the workload needs
/// one that shows its own processes, with the workload as PID 1.
fn mount_workload_proc() {
    let mounted = mount_fs(
        Some(c"proc"),
        "/proc",
        Some(c"proc"),
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        None,
    );
    if let Err(err) = mounted {
        // A boot without /proc is degraded but still better than not starting
        // the workload at all.
        warn!(error = %err, "cannot mount the workload's /proc");
    }
}

/// Mounts `/proc`, `/sys` and `/dev` before anything can need them.
fn mount_early_filesystems() -> io::Result<()> {
    for directory in ["/proc", "/sys", "/dev", ROOT_MOUNT] {
        fs::create_dir_all(directory)
            .map_err(|err| with_context(err, format!("creating {directory}")))?;
    }
    // The initramfs root must not propagate mounts into the image root.
    mount_fs(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    mount_or_busy(c"devtmpfs", "/dev", c"devtmpfs", libc::MS_NOSUID, None)?;
    mount_or_busy(
        c"proc",
        "/proc",
        c"proc",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        None,
    )?;
    mount_or_busy(
        c"sysfs",
        "/sys",
        c"sysfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        None,
    )?;
    Ok(())
}

/// Mounts the image, reads the workload config, then makes the image root this
/// process's root.
struct Workload {
    process: ImageConfig,
    stdin: bool,
    network: Vec<net::InterfaceConfig>,
    hostname: Option<String>,
}

/// Sets the UTS hostname the workload and its children see.
fn set_hostname(name: &str) -> io::Result<()> {
    // SAFETY: the name is a valid byte slice and its length is passed.
    let result = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Parses the document's static interface configuration.
fn interface_configs(document: &ImageConfigDocument) -> io::Result<Vec<net::InterfaceConfig>> {
    document
        .network()
        .iter()
        .map(|nic| {
            let addresses = nic
                .addresses
                .iter()
                .map(|address| {
                    address.parse().map_err(|err| {
                        with_context(err, format!("address {address:?} on {}", nic.mac))
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            let gateway = nic
                .gateway
                .as_deref()
                .map(|gateway| {
                    gateway.parse::<std::net::IpAddr>().map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid gateway {gateway:?} on {}: {err}", nic.mac),
                        )
                    })
                })
                .transpose()?;
            Ok(net::InterfaceConfig {
                mac: nic.mac.clone(),
                addresses,
                gateway,
                mtu: nic.mtu,
            })
        })
        .collect()
}

fn mount_image() -> io::Result<Workload> {
    mount_fs(Some(ROOTFS_TAG), ROOT_MOUNT, Some(c"virtiofs"), 0, None)
        .map_err(|err| with_context(err, format!("mounting the image at {ROOT_MOUNT}")))?;
    // The config document is part of the image root; read it here, while the
    // mount is still reachable from the initramfs root. It also carries the
    // writable-layer contract for an ephemeral root.
    let document =
        read_config().map_err(|err| with_context(err, format!("reading {CONFIG_FILE}")))?;
    let stdin = document.stdin();
    let upper_bytes = document.tmpfs_upper_bytes();
    let network = interface_configs(&document).map_err(|err| {
        with_context(
            err,
            format!("reading the static network configuration from {CONFIG_FILE}"),
        )
    })?;
    let hostname = document.hostname().map(str::to_owned);
    let config = document.config;

    // The workload root: the image share itself, or an overlay that keeps the
    // image read-only and puts every write in this VM's own tmpfs.
    let root = match upper_bytes {
        Some(bytes) => mount_ephemeral_root(bytes)?,
        None => ROOT_MOUNT.to_owned(),
    };

    for mount in ["/proc", "/sys", "/dev"] {
        early_mount(&root, mount)?;
    }

    switch_root(&root)?;
    let args = config.args();
    let program = args
        .first()
        .cloned()
        .unwrap_or_else(|| FALLBACK_INIT.join(" or "));
    info!(
        %program,
        environment = config.env().len(),
        "image mounted; running workload"
    );
    Ok(Workload {
        process: config,
        stdin,
        network,
        hostname,
    })
}

/// Moves an early-boot mount under the image root.
fn early_mount(root: &str, mount: &str) -> io::Result<()> {
    let target = format!("{root}{mount}");
    if let Err(err) = fs::create_dir_all(&target) {
        // A read-only image without these mount points cannot receive them;
        // warn and keep going rather than refusing to boot.
        warn!(%target, mount = %mount, %err, "cannot create mount target; skipping the mount");
        return Ok(());
    }
    let source = cstring(mount, "mount source")?;
    mount_fs(Some(&source), &target, None, libc::MS_MOVE, None)
        .map_err(|err| with_context(err, format!("moving {mount} into the image")))
}

/// Builds the merged root of an ephemeral VM: the image share is the read-only
/// overlay lower and a size-bounded tmpfs holds the writable upper, so guest
/// writes are charged to this VM instead of host memory. `redirect_dir=on`
/// keeps directory renames working against image content.
fn mount_ephemeral_root(upper_bytes: u64) -> io::Result<String> {
    create_directories([UPPER_MOUNT, OVERLAY_MOUNT])?;

    let tmpfs_options = cstring(&format!("size={upper_bytes},mode=0700"), "tmpfs options")?;
    mount_fs(
        Some(c"tmpfs"),
        UPPER_MOUNT,
        Some(c"tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&tmpfs_options),
    )
    .map_err(|err| with_context(err, format!("mounting the {upper_bytes}-byte tmpfs upper")))?;

    create_directories([UPPER_DIR, WORK_DIR])?;

    let overlay_options = cstring(
        &format!("lowerdir={ROOT_MOUNT},upperdir={UPPER_DIR},workdir={WORK_DIR},redirect_dir=on"),
        "overlay options",
    )?;
    mount_fs(
        Some(c"overlay"),
        OVERLAY_MOUNT,
        Some(c"overlay"),
        0,
        Some(&overlay_options),
    )
    .map_err(|err| with_context(err, format!("mounting the overlay at {OVERLAY_MOUNT}")))?;

    info!(upper_bytes, root = %ROOT_MOUNT, "ephemeral root merged");
    Ok(OVERLAY_MOUNT.to_owned())
}

fn create_directories<const N: usize>(directories: [&str; N]) -> io::Result<()> {
    for directory in directories {
        fs::create_dir_all(directory)
            .map_err(|err| with_context(err, format!("creating {directory}")))?;
    }
    Ok(())
}

fn read_config() -> io::Result<ImageConfigDocument> {
    let mut file = fs::File::open(CONFIG_FILE)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{CONFIG_FILE} is larger than {MAX_CONFIG_BYTES} bytes"),
        ));
    }
    ImageConfigDocument::parse(&bytes)
}

/// Makes `root` the workload root. The initramfs root is `rootfs`, which
/// `pivot_root(2)` refuses (`!mnt_has_parent(root_mnt)` refuses the absolute
/// root) and which cannot be unmounted, so this is the documented handoff from
/// `ramfs-rootfs-initramfs.rst`: overmount rootfs with the new root and chroot
/// into it. The image share (and an ephemeral root's tmpfs upper) hangs off
/// rootfs and becomes unreachable at the same time.
fn switch_root(root: &str) -> io::Result<()> {
    std::env::set_current_dir(root).map_err(|err| with_context(err, format!("entering {root}")))?;
    let source = cstring(".", "switch root")?;
    let root_slash = cstring("/", "switch root target")?;
    // SAFETY: every pointer is either null or a valid NUL-terminated string.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            root_slash.as_ptr(),
            std::ptr::null(),
            libc::MS_MOVE,
            std::ptr::null(),
        )
    } < 0
    {
        return Err(with_context(
            io::Error::last_os_error(),
            "moving the workload root over the initramfs root",
        ));
    }
    // SAFETY: "." resolves inside the new root, with a NUL terminator added.
    if unsafe { libc::chroot(c".".as_ptr()) } < 0 {
        return Err(with_context(
            io::Error::last_os_error(),
            "chrooting into the workload root",
        ));
    }
    std::env::set_current_dir("/").map_err(|err| with_context(err, "setting / as cwd"))?;
    info!("switched to the workload root; the initramfs mount is hidden underneath");
    Ok(())
}

fn mount_fs(
    source: Option<&CStr>,
    target: &str,
    fstype: Option<&CStr>,
    flags: libc::c_ulong,
    data: Option<&CStr>,
) -> io::Result<()> {
    let target = cstring(target, "mount point")?;
    // SAFETY: every pointer is either null or a valid NUL-terminated string,
    // and the kernel only reads them for the duration of the call.
    let result = unsafe {
        libc::mount(
            source.map_or(std::ptr::null(), CStr::as_ptr),
            target.as_ptr(),
            fstype.map_or(std::ptr::null(), CStr::as_ptr),
            flags,
            data.map_or(std::ptr::null(), |data| data.as_ptr().cast()),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn mount_or_busy(
    source: &CStr,
    target: &str,
    fstype: &CStr,
    flags: libc::c_ulong,
    data: Option<&CStr>,
) -> io::Result<()> {
    match mount_fs(Some(source), target, Some(fstype), flags, data) {
        Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        result => result,
    }
}

fn signal_pipe() -> io::Result<RawFd> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes exactly two file descriptors into fds.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    SIGNAL_FD.store(fds[1], Ordering::Relaxed);
    Ok(fds[0])
}

fn install_signal_handlers() -> io::Result<()> {
    // SAFETY: sigaction is a plain C struct; zeroing it is a valid initial
    // state before the fields below are filled in.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: action.sa_mask is a valid sigset owned by action.
    unsafe { libc::sigemptyset(std::ptr::from_mut(&mut action.sa_mask)) };
    for signal in HANDLED_SIGNALS {
        // SAFETY: action is fully initialized and lives for the call.
        if unsafe { libc::sigaction(signal, std::ptr::from_ref(&action), std::ptr::null_mut()) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn handle_signal(signal: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = [u8::try_from(signal).unwrap_or(u8::MAX)];
    // SAFETY: write is async-signal-safe and the pipe fd stays open for the
    // life of the process.
    unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
}

/// Reaps children and forwards shutdown signals until the workload exits.
fn supervise(child: libc::pid_t, read_fd: RawFd) -> io::Result<ExitInfo> {
    let mut status = None;
    let mut buffer = [0_u8; 64];
    let mut kill_at: Option<Instant> = None;
    loop {
        reap_children(child, &mut status)?;
        if let Some(info) = status {
            return Ok(info);
        }
        let timeout = kill_at.map_or(-1, |deadline| {
            i32::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(i32::MAX)
        });
        let mut poll_fd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd is one initialized pollfd and poll writes only to it.
        let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd), 1, timeout) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            // The workload ignored the forwarded termination signal; SIGKILL
            // cannot be ignored by a namespace init.
            signal_workload(child, libc::SIGKILL);
            kill_at = None;
            continue;
        }
        // SAFETY: buffer is valid for buffer.len() bytes of writes.
        let count = unsafe { libc::read(read_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(err);
        }
        let count = usize::try_from(count).unwrap_or(0);
        for &signal in &buffer[..count] {
            let signal = libc::c_int::from(signal);
            if matches!(signal, libc::SIGTERM | libc::SIGINT | libc::SIGQUIT) {
                kill_at = Instant::now().checked_add(SHUTDOWN_GRACE);
            }
            handle_pending_signal(signal, child);
        }
    }
}

fn handle_pending_signal(signal: libc::c_int, child: libc::pid_t) {
    if signal == libc::SIGCHLD {
        return;
    }
    signal_workload(child, signal);
}

fn signal_workload(child: libc::pid_t, signal: libc::c_int) {
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "a child pid always fits a negated pid_t"
    )]
    // SAFETY: a negated pid signals the child's process group; failure is
    // expected once the workload has already exited.
    let _sent = unsafe { libc::kill(-child, signal) };
}

fn reap_children(child: libc::pid_t, status: &mut Option<ExitInfo>) -> io::Result<()> {
    loop {
        let mut wait_status = 0;
        // SAFETY: wait_status is a valid out-parameter for waitpid.
        let pid = unsafe { libc::waitpid(-1, std::ptr::from_mut(&mut wait_status), libc::WNOHANG) };
        if pid == 0 {
            return Ok(());
        }
        if pid < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ECHILD) => return Ok(()),
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
        if pid == child {
            *status = Some(ExitInfo::from_wait_status(wait_status));
        }
    }
}

/// Data about exit codes
struct ExitInfo {
    code: i32,
    description: String,
}

impl ExitInfo {
    fn from_wait_status(status: libc::c_int) -> Self {
        if libc::WIFEXITED(status) {
            let code = libc::WEXITSTATUS(status);
            Self {
                code,
                description: format!("exited with code {code}"),
            }
        } else if libc::WIFSIGNALED(status) {
            let signal = libc::WTERMSIG(status);
            let name = signal_name(signal);
            Self {
                code: 128_i32.saturating_add(signal),
                description: format!("was killed by {name} ({signal})"),
            }
        } else {
            Self {
                code: 1,
                description: format!("ended with wait status {status}"),
            }
        }
    }
}

const fn signal_name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGHUP => "SIGHUP",
        _ => "a signal",
    }
}

/// Replaces this process with the workload. Only returns on failure.
fn exec_workload(config: &ImageConfig) -> io::Result<Infallible> {
    let env = build_env(config)?;
    let command = config.args();
    let command = if command.is_empty() {
        vec![find_fallback_init()?.to_owned()]
    } else {
        command
    };
    let program = resolve_program(&command[0], &env)?;

    std::env::set_current_dir(config.working_dir()).map_err(|err| {
        with_context(
            err,
            format!("entering working directory {:?}", config.working_dir()),
        )
    })?;
    if let Some(spec) = config.user_spec() {
        apply_identity(&resolve_identity(spec)?)?;
    }

    let arguments: Vec<CString> = command
        .iter()
        .map(|argument| cstring(argument, "argument"))
        .collect::<io::Result<_>>()?;

    let mut argument_ptrs: Vec<*const libc::c_char> =
        arguments.iter().map(|argument| argument.as_ptr()).collect();
    argument_ptrs.push(std::ptr::null());

    let env: Vec<CString> = env
        .iter()
        .map(|entry| cstring(entry, "environment entry"))
        .collect::<io::Result<_>>()?;

    let mut env_ptrs: Vec<*const libc::c_char> = env.iter().map(|entry| entry.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    // SAFETY: arguments and env are NUL-terminated arrays of NUL-terminated
    // strings, and program is an absolute or PATH-resolved path.
    unsafe { libc::execve(program.as_ptr(), argument_ptrs.as_ptr(), env_ptrs.as_ptr()) };
    Err(io::Error::last_os_error())
}

/// build environment variables to pass onto microvm
fn build_env(config: &ImageConfig) -> io::Result<Vec<String>> {
    let mut env = Vec::with_capacity(config.env().len().saturating_add(1));
    for entry in config.env() {
        if !entry.contains('=') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("environment entry {entry:?} has no '='"),
            ));
        }
        env.push(entry.clone());
    }
    if !env.iter().any(|entry| entry.starts_with("PATH=")) {
        env.push(format!("PATH={DEFAULT_PATH}"));
    }
    Ok(env)
}

fn resolve_program(program: &str, env: &[String]) -> io::Result<CString> {
    if program.contains('/') {
        return cstring(program, "program");
    }
    let path = env
        .iter()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .unwrap_or(DEFAULT_PATH);
    for directory in path.split(':').filter(|directory| !directory.is_empty()) {
        let candidate = format!("{directory}/{program}");
        if is_executable(&candidate) {
            return cstring(&candidate, "program");
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("program {program:?} not found in PATH"),
    ))
}

fn find_fallback_init() -> io::Result<&'static str> {
    FALLBACK_INIT
        .into_iter()
        .find(|candidate| is_executable(candidate))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("none of {} exists in the image", FALLBACK_INIT.join(", ")),
            )
        })
}

fn is_executable(path: &str) -> bool {
    let Ok(path) = CString::new(path) else {
        return false;
    };
    // SAFETY: path is a valid NUL-terminated string.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

struct Identity {
    uid: libc::uid_t,
    gid: libc::gid_t,
}

fn resolve_identity(spec: &str) -> io::Result<Identity> {
    let (user, group) = spec
        .split_once(':')
        .map_or((spec, None), |(user, group)| (user, Some(group)));
    let (uid, primary_gid) = if let Ok(uid) = user.parse::<libc::uid_t>() {
        // SAFETY: getpwuid returns a pointer to libc-owned static storage that
        // stays valid until the next lookup.
        let gid = unsafe { libc::getpwuid(uid).as_ref() }.map_or(0, |entry| entry.pw_gid);
        (uid, gid)
    } else {
        let name = c_name("user", user)?;
        // SAFETY: the name is NUL-terminated; the returned entry is valid until
        // the next lookup.
        let entry = unsafe { libc::getpwnam(name.as_ptr()).as_ref() }
            .ok_or_else(|| not_found(format!("user {user:?} is not in /etc/passwd")))?;
        (entry.pw_uid, entry.pw_gid)
    };
    let gid = match group.filter(|group| !group.is_empty()) {
        Some(group) => {
            if let Ok(gid) = group.parse::<libc::gid_t>() {
                gid
            } else {
                let name = c_name("group", group)?;
                // SAFETY: the name is NUL-terminated; the returned entry is
                // valid until the next lookup.
                let entry = unsafe { libc::getgrnam(name.as_ptr()).as_ref() }
                    .ok_or_else(|| not_found(format!("group {group:?} is not in /etc/group")))?;
                entry.gr_gid
            }
        }
        None => primary_gid,
    };
    Ok(Identity { uid, gid })
}

fn c_name(kind: &str, name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{kind} {name:?} contains a NUL byte"),
        )
    })
}

fn apply_identity(identity: &Identity) -> io::Result<()> {
    let groups = [identity.gid];
    // SAFETY: the array is valid for groups.len() entries.
    if unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } < 0 {
        return Err(with_context(
            io::Error::last_os_error(),
            "setting supplementary groups",
        ));
    }
    // SAFETY: setgid/setuid take plain integers.
    if unsafe { libc::setgid(identity.gid) } < 0 {
        return Err(with_context(io::Error::last_os_error(), "setting gid"));
    }
    // SAFETY: setuid is called after setgid so the uid change is not lost.
    if unsafe { libc::setuid(identity.uid) } < 0 {
        return Err(with_context(io::Error::last_os_error(), "setting uid"));
    }
    Ok(())
}

fn power_off(code: i32) -> ! {
    info!(status = code, "powering off");
    // SAFETY: PID 1 with CAP_SYS_BOOT may reboot, and RB_POWER_OFF is valid.
    if unsafe { libc::reboot(libc::RB_POWER_OFF) } < 0 {
        error!(
            error = %io::Error::last_os_error(),
            "poweroff failed; rebooting"
        );
        // SAFETY: a restart is the next best way to end the VM.
        unsafe { libc::reboot(libc::RB_AUTOBOOT) };
    }
    // SAFETY: _exit terminates PID 1 without running destructors.
    unsafe { libc::_exit(code) }
}

fn cstring(value: &str, what: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} contains a NUL byte: {value:?}"),
        )
    })
}

fn not_found(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, message)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "callers hand over the error they already built"
)]
fn with_context(err: io::Error, context: impl fmt::Display) -> io::Error {
    io::Error::new(err.kind(), format!("{context}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_config(env: &[&str]) -> ImageConfig {
        ImageConfig {
            env: Some(env.iter().map(|entry| (*entry).to_owned()).collect()),
            ..ImageConfig::default()
        }
    }

    #[test]
    fn adds_default_path_when_missing() {
        let env = build_env(&image_config(&["HOME=/root"])).unwrap();
        assert_eq!(
            env,
            ["HOME=/root".to_owned(), format!("PATH={DEFAULT_PATH}")]
        );
    }

    #[test]
    fn keeps_image_path() {
        assert_eq!(
            build_env(&image_config(&["PATH=/bin"])).unwrap(),
            ["PATH=/bin"]
        );
    }

    #[test]
    fn rejects_env_without_equals() {
        build_env(&image_config(&["BROKEN"])).unwrap_err();
    }

    #[test]
    fn resolves_numeric_identity_with_explicit_group() {
        let identity = resolve_identity("0:1234").unwrap();
        assert_eq!(identity.uid, 0);
        assert_eq!(identity.gid, 1234);
    }

    #[test]
    fn maps_signal_exit_codes() {
        let info = ExitInfo::from_wait_status(libc::SIGTERM);
        assert_eq!(info.code, 128 + libc::SIGTERM);
        assert!(info.description.contains("SIGTERM"));
    }

    #[test]
    fn resolves_relative_programs_through_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("hello");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = [format!("PATH={}", dir.path().display())];
        let resolved = resolve_program("hello", &env).unwrap();
        assert_eq!(resolved.to_str().unwrap(), program.to_str().unwrap());
    }
}
