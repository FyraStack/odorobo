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

## virtio-console socket

To connect to the virtio-console socket directly, you will need to find the PTY allocated for it

```bash
odoroboctl info my-vm
```

Look at the `config.console` section of the output, and find the `path` field. This is the path to the PTY on the host that is connected to the VM's virtio-console. You can connect to this socket with `screen` or `minicom` or any other terminal program:

```bash
screen /dev/pts/N
```

Where `N` is the number from the `path` field in the `config.console` section of the `odoroboctl info` output.

odoroboctl should implement the WebSocket tty proxy later, but for now this is how you get a shell on the console directly