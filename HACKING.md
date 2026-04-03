## agetty-over-vsock

To create a fresh agetty session over vsock, you can do this:

on the guest:
```bash
# optional: set SELinux to permissive so it doesn't interfere with the pty creation
# consider writing some kind of policy to fix this properly, but for quick testing this is fine
sudo setenforce 0
# run socat to listen on vsock port 1234 and spawn agetty for each connection, as root
sudo socat VSOCK-LISTEN:1234,reuseaddr,fork EXEC:'/sbin/agetty --noclear -J - xterm-256color',pty,setsid,ctty,raw,echo=0
```

on the host:
```bash
# use the vsock script, script 
python scripts/vsock.py my-vm
```

Fyra Stack production images should just have a management agent to talk over vsock and do other stuff,
but this is a quick way to get a shell over vsock for testing and debugging.

## serial console (PTY)

Cloud Hypervisor allocates a PTY for the serial console. The PTY path is reported via the CH API after the VM is created.

```bash
odoroboctl info my-vm
```

Look at the `config.serial` section of the output and find the `path` field. This is the PTY on the host connected to the VM's serial console. Connect to it with `screen` or any other terminal program:

```bash
screen /dev/pts/N
```

Where `N` is the number from the `path` field.

Alternatively, use the agent's WebSocket console proxy from any machine that can reach the agent:

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

Note: the PTY path changes every time the VM is restarted or the VMM process is recreated. It is not stable across live migrations.