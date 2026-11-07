//! PID 1 behaviour: mount the image, start the workload, supervise the VM.

use std::convert::Infallible;
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs;
use std::io::{self, Read as _};
use std::os::fd::RawFd;
use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::image::{ImageConfig, ImageConfigDocument};

const ROOTFS_TAG: &CStr = c"rootfs";
const ROOT_MOUNT: &str = "/newroot";
/// Guest-private tmpfs holding an ephemeral root's writable overlay layer.
const UPPER_MOUNT: &str = "/upper";
const UPPER_DIR: &str = "/upper/rootfs.upper";
const WORK_DIR: &str = "/upper/rootfs.work";
/// Merged root of an ephemeral VM: the image share plus the tmpfs upper.
const OVERLAY_MOUNT: &str = "/overlay";
/// Directory the old (initramfs) root is parked on during the switch; it must
/// be writable, so a read-only root parks it under its `/tmp` scratch mount.
const OLD_ROOT: &str = ".odorobo-oldroot";
const OLD_ROOT_SCRATCH: &str = "tmp/.odorobo-oldroot";
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

/// Write end of the self-pipe the signal handler reports through.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a missing console must not stop the init"
        )]
        let _ = writeln!(std::io::stderr(), "odorobo-init: {}", format_args!($($arg)*));
    }};
}

/// Runs as PID 1 and never returns on success: the VM is powered off when the
/// workload exits.
pub fn run() -> ExitCode {
    // SAFETY: getpid takes no arguments and cannot fail.
    let pid = unsafe { libc::getpid() };
    if pid != 1 {
        log!("must run as PID 1, not {pid}");
        return ExitCode::FAILURE;
    }
    restore_console();
    match run_pid1() {
        Ok(code) => power_off(code),
        Err(err) => {
            log!("fatal: {err}");
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
    let read_fd = signal_pipe()?;
    install_signal_handlers()?;

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
            log!(
                "workload started with pid {pid}: {:?}",
                workload.process.args()
            );
            let info = supervise(pid, read_fd)?;
            log!("workload {} (status {})", info.description, info.code);
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
        log!(
            "cannot open /dev/hvc0: {}; the workload keeps the init console",
            io::Error::last_os_error()
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
            log!(
                "cannot open /dev/null: {}; closing the workload's stdin",
                io::Error::last_os_error()
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
        log!(
            "workload console cannot become the controlling terminal: {}",
            io::Error::last_os_error()
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
    use std::io::Write as _;
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a missing console must not stop the init"
    )]
    let _ = std::io::stdout().write_all(&text);
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
    match exec_workload(&workload.process) {
        Ok(never) => match never {},
        Err(err) => {
            log!("cannot start workload: {err}");
            // SAFETY: _exit terminates the child without running destructors.
            unsafe { libc::_exit(127) }
        }
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
    let config = document.config;

    // The workload root: the image share itself, or an overlay that keeps the
    // image read-only and puts every write in this VM's own tmpfs.
    let root = match upper_bytes {
        Some(bytes) => mount_ephemeral_root(bytes)?,
        None => ROOT_MOUNT.to_owned(),
    };

    for mount in ["/proc", "/sys", "/dev"] {
        let target = format!("{root}{mount}");
        if let Err(err) = fs::create_dir_all(&target) {
            // A read-only image without these mount points cannot receive
            // them; warn and keep going rather than refusing to boot.
            log!("cannot create {target}: {err}; skipping the {mount} mount");
            continue;
        }
        let source = cstring(mount, "mount source")?;
        mount_fs(Some(&source), &target, None, libc::MS_MOVE, None)
            .map_err(|err| with_context(err, format!("moving {mount} into the image")))?;
    }

    switch_root(&root)?;
    let args = config.args();
    log!(
        "image mounted; running {} with {} environment entries",
        if args.is_empty() {
            FALLBACK_INIT.join(" or ")
        } else {
            args[0].clone()
        },
        config.env().len()
    );
    Ok(Workload {
        process: config,
        stdin,
    })
}

/// Builds the merged root of an ephemeral VM: the image share is the read-only
/// overlay lower and a size-bounded tmpfs holds the writable upper, so guest
/// writes are charged to this VM instead of host memory. `redirect_dir=on`
/// keeps directory renames working against image content.
fn mount_ephemeral_root(upper_bytes: u64) -> io::Result<String> {
    for directory in [UPPER_MOUNT, OVERLAY_MOUNT] {
        fs::create_dir_all(directory)
            .map_err(|err| with_context(err, format!("creating {directory}")))?;
    }
    let options = cstring(&format!("size={upper_bytes},mode=0700"), "tmpfs options")?;
    mount_fs(
        Some(c"tmpfs"),
        UPPER_MOUNT,
        Some(c"tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&options),
    )
    .map_err(|err| with_context(err, format!("mounting the {upper_bytes}-byte tmpfs upper")))?;
    for directory in [UPPER_DIR, WORK_DIR] {
        fs::create_dir_all(directory)
            .map_err(|err| with_context(err, format!("creating {directory}")))?;
    }
    let data = cstring(
        &format!("lowerdir={ROOT_MOUNT},upperdir={UPPER_DIR},workdir={WORK_DIR},redirect_dir=on"),
        "overlay options",
    )?;
    mount_fs(
        Some(c"overlay"),
        OVERLAY_MOUNT,
        Some(c"overlay"),
        0,
        Some(&data),
    )
    .map_err(|err| with_context(err, format!("mounting the overlay at {OVERLAY_MOUNT}")))?;
    log!("ephemeral root merged from {ROOT_MOUNT} with a {upper_bytes}-byte tmpfs upper");
    Ok(OVERLAY_MOUNT.to_owned())
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

/// Makes `root` the workload root: `pivot_root(2)` into it, then lazily
/// unmount the old (initramfs) tree, which also takes the image share and an
/// ephemeral root's tmpfs upper out of the workload's mount namespace.
fn switch_root(root: &str) -> io::Result<()> {
    std::env::set_current_dir(root)
        .map_err(|err| with_context(err, format!("entering {root}")))?;
    // pivot_root parks the old tree on a directory inside the new root, so
    // that directory has to be writable: the /tmp scratch mount of a read-only
    // root standing in for the image filesystem.
    let parked = match fs::create_dir_all(OLD_ROOT) {
        Ok(()) => OLD_ROOT,
        Err(_) => {
            fs::create_dir_all(OLD_ROOT_SCRATCH)
                .map_err(|err| with_context(err, format!("creating {OLD_ROOT_SCRATCH}")))?;
            OLD_ROOT_SCRATCH
        }
    };
    let new_root = cstring(".", "pivot root")?;
    let put_old = cstring(parked, "pivot root put_old")?;
    // SAFETY: both pointers are valid NUL-terminated path strings, and the
    // kernel only reads them for the duration of the call.
    if unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) } < 0 {
        let err = io::Error::last_os_error();
        return Err(with_context(err, "pivoting into the workload root"));
    }
    unmount_lazy(parked)
        .map_err(|err| with_context(err, format!("unmounting the old root at {parked}")))?;
    // The old tree is out of the namespace; drop the parking directory.
    _ = fs::remove_dir(parked);
    std::env::set_current_dir("/").map_err(|err| with_context(err, "setting / as cwd"))?;
    log!("pivoted into the workload root; the old root is detached");
    Ok(())
}

/// `umount2(2)` with `MNT_DETACH`: the mount leaves the namespace now and is
/// released once its last user (an overlay lower or upper, say) releases it.
fn unmount_lazy(path: &str) -> io::Result<()> {
    let target = cstring(path, "unmount target")?;
    // SAFETY: the pointer is a valid NUL-terminated path string.
    if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
    loop {
        reap_children(child, &mut status)?;
        if let Some(info) = status {
            return Ok(info);
        }
        let mut poll_fd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd is one initialized pollfd and poll writes only to it.
        let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd), 1, -1) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
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
            handle_pending_signal(libc::c_int::from(signal), child);
        }
    }
}

fn handle_pending_signal(signal: libc::c_int, child: libc::pid_t) {
    if signal == libc::SIGCHLD {
        return;
    }
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
    let passwd = read_database("/etc/passwd", parse_passwd)?;
    let (uid, primary_gid) = if let Ok(uid) = user.parse::<libc::uid_t>() {
        let gid = passwd
            .iter()
            .find(|entry| entry.id == uid)
            .map_or(0, |entry| entry.group);
        (uid, gid)
    } else {
        let entry = passwd
            .iter()
            .find(|entry| entry.name == user)
            .ok_or_else(|| not_found(format!("user {user:?} is not in /etc/passwd")))?;
        (entry.id, entry.group)
    };
    let gid = match group.filter(|group| !group.is_empty()) {
        Some(group) => match group.parse::<libc::gid_t>() {
            Ok(gid) => gid,
            Err(_) => read_database("/etc/group", parse_group)?
                .into_iter()
                .find(|entry| entry.name == group)
                .map(|entry| entry.id)
                .ok_or_else(|| not_found(format!("group {group:?} is not in /etc/group")))?,
        },
        None => primary_gid,
    };
    Ok(Identity { uid, gid })
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

struct DatabaseEntry {
    name: String,
    id: u32,
    group: u32,
}

fn read_database(
    path: &str,
    parse: fn(&str) -> Vec<DatabaseEntry>,
) -> io::Result<Vec<DatabaseEntry>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(parse(&contents)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(with_context(err, format!("reading {path}"))),
    }
}

fn parse_passwd(contents: &str) -> Vec<DatabaseEntry> {
    contents
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split(':');
            Some(DatabaseEntry {
                name: fields.next()?.to_owned(),
                id: fields.nth(1)?.parse().ok()?,
                group: fields.next()?.parse().ok()?,
            })
        })
        .collect()
}

fn parse_group(contents: &str) -> Vec<DatabaseEntry> {
    contents
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split(':');
            Some(DatabaseEntry {
                name: fields.next()?.to_owned(),
                id: fields.nth(1)?.parse().ok()?,
                group: 0,
            })
        })
        .collect()
}

fn power_off(code: i32) -> ! {
    log!("powering off (status {code})");
    // SAFETY: PID 1 with CAP_SYS_BOOT may reboot, and RB_POWER_OFF is valid.
    if unsafe { libc::reboot(libc::RB_POWER_OFF) } < 0 {
        log!("poweroff failed: {}", io::Error::last_os_error());
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
    fn parses_passwd_without_comments() {
        let entries = parse_passwd(
            "# comment\nroot:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534::/:/sbin/nologin",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].name.as_str(), entries[0].id, entries[0].group),
            ("root", 0, 0)
        );
        assert_eq!(entries[1].id, 65534);
    }

    #[test]
    fn parses_group_names() {
        let entries = parse_group("root:x:0:\ntty:x:5:user");
        assert_eq!(entries[1].name, "tty");
        assert_eq!(entries[1].id, 5);
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
