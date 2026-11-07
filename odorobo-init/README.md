# odorobo-init

PID 1 for rootfs-backed microVMs. It mounts the image, applies the image's OCI
process config, runs the workload, and powers the VM off when the workload
exits. The static binary is packed as `/init` into
`/var/lib/odorobo/microvm/initramfs.cpio.gz`; the kernel boots it directly, and
the init mounts the root filesystem itself.

## How the OCI config reaches the guest

Cloud Hypervisor has no payload channel besides the initramfs, so the config
arrives on a second read-only virtiofs share.

1. Image preparation reads the config blob (`read_oci_manifest` in
   `odorobo/src/ch_driver/containers/oci.rs`) and keeps the bytes in
   `PreparedImage::config`.
2. `VMActor::start` writes them to
   `/run/odorobo/vms/<vmid>/config/config.json` before creating the VM
   (`odorobo/src/ch_driver/actor.rs`).
3. A second `virtiofsd` serves that directory with tag `odorobo-config` and
   socket `/run/odorobo/vms/<vmid>/odorobo-config.sock`.
4. `odorobo/src/ch_driver/manifest.rs` adds the matching `FsConfig`, so the
   guest sees two shares: `rootfs` and `odorobo-config`.
5. The init mounts the config share read-only at `/run/.odorobo`, reads
   `/run/.odorobo/config.json` (1 MiB limit), and parses it before mounting the
   image at `/newroot`.

The bytes are not rewritten. The init uses only the `config` object and ignores
`architecture`, `os`, `rootfs`, and `history`. A missing share, unreadable file,
invalid JSON, or file over 1 MiB is fatal: the init logs
`odorobo-init: fatal: ...` and powers off.

```json
{
  "architecture": "amd64",
  "os": "linux",
  "config": {
    "Entrypoint": ["/usr/sbin/nginx", "-g", "daemon off;"],
    "Cmd": [],
    "Env": ["NGINX_VERSION=1.27.4", "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "WorkingDir": "/",
    "User": "www-data"
  },
  "rootfs": { "type": "layers", "diff_ids": ["sha256:..."] }
}
```

No manifest field overrides the image's `Env`, `Cmd`, or `User` yet.

## Container options

| OCI field | Effect |
| --- | --- |
| `Entrypoint` | First part of argv. |
| `Cmd` | Appended to `Entrypoint`. If `Entrypoint` is absent, `Cmd` alone is argv. |
| `Env` | Environment for the workload, in the given order. |
| `WorkingDir` | `chdir` target before exec; empty or absent means `/`. |
| `User` | `uid[:gid]` or `name[:group]`, resolved against the image's `/etc/passwd` and `/etc/group`; absent means uid/gid 0. |

- **argv.** `Entrypoint` and `Cmd` are concatenated as-is; each element is one
  argv entry. There is no shell, no word splitting, and no `ArgsEscaped`
  handling. With neither field set, the init tries `/sbin/init`, `/etc/init`,
  `/bin/init`, `/bin/sh`.
- **PATH.** If `Env` has no `PATH=`, the init appends
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`. A program name
  containing `/` is used as-is; otherwise the init searches `PATH`. A missing
  executable exits the workload with 127.
- **Env.** Entries without `=` are rejected as invalid config. Only the image's
  environment is passed; the init adds nothing else (no `HOME`, `HOSTNAME`, or
  `TERM`).
- **User.** A numeric uid without a passwd entry gets gid 0. Supplementary
  groups are reset to the single resolved gid, and the switch happens after
  mounts and chroot, before `execve`. Names are looked up inside the image, not
  on the host.
- **Ignored.** `Labels`, `ExposedPorts`, `Volumes`, `StopSignal`, `Healthcheck`,
  `Shell`, `ArgsEscaped`, and `OnBuild` do nothing. Networking, disk, and layer
  merging happen on the host; there is no container runtime in the guest.

## Boot sequence

1. The kernel runs `/init`; it recreates `/dev/console` and takes over stdio.
2. It marks its mounts private, then mounts `devtmpfs`, `proc`, and `sysfs`.
3. It mounts `odorobo-config` and parses `config.json`.
4. It mounts `rootfs` at `/newroot`, moves the pseudo filesystems in, and
   chroots to `/`.
5. It forks, applies `User`, and execs `Entrypoint` + `Cmd`.
6. It reaps children, forwards `SIGTERM`, `SIGINT`, `SIGQUIT`, and `SIGHUP` to
   the process group, and powers off when the main child exits. The child's
   status is logged.

## Building

```sh
just initramfs
```

This builds the musl binary, packs `init` into a deterministic newc cpio.gz, and
installs it with `sudo` to `/var/lib/odorobo/microvm/initramfs.cpio.gz`. The
archive holds only `init`. The root `justfile` has the same recipe.

## Debugging

- Console (`console=ttyS0`): kernel messages, then `odorobo-init:` lines.
- Input: `/run/odorobo/vms/<vmid>/config/config.json`.
- Share log: `/run/odorobo/vms/<vmid>/virtiofsd-config.log`.
- Boot test: `cargo test -p odorobo --bin odorobo faas_vm_boots -- --ignored --nocapture`.
  It uses a fixture image with `Entrypoint: ["/bin/sh"]` and needs `/dev/kvm` and
  root access to `/var/lib/odorobo`.
