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

## serial console socket

Cloud Hypervisor is configured to expose the serial console as a Unix domain socket at a stable, predictable path:

```
/run/odorobo/vms/<vmid>/console.sock
```

Connect directly on the host with socat:

```bash
# raw output only (useful for scripting or piping)
socat - UNIX-CONNECT:/run/odorobo/vms/my-vm/console.sock

# interactive session with local terminal in raw mode
socat file:`tty`,raw,echo=0 UNIX-CONNECT:/run/odorobo/vms/my-vm/console.sock
```

Press `Ctrl-]` then `q` to exit socat in interactive mode, or just close the terminal.

Alternatively, use the agent's WebSocket console proxy from any machine that can reach the agent:

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

The socket path is stable across live migrations — after a migration completes, reconnect to the same path on the destination node.