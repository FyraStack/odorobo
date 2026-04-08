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

Look at the `config.serial` section of the output and find the `file` field. This is the PTY on the host connected to the VM's serial console. Connect to it with `screen` or any other terminal program:

```bash
screen /dev/pts/N
```

Where `N` is the number from the `path` field.

Alternatively, use the agent's WebSocket console proxy from any machine that can reach the agent:

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

Note: the PTY path changes every time the VM is restarted or the VMM process is recreated. It is not stable across live migrations.

## `async_trait` usage

While Rust 1.75 (2024) finally adds support for async functions in traits, Odorobo makes extensive usage
of dynamic dispatch for config hooks and config transformers, which are still not supported by Rust's async fn in traits.

To work around this, Odorobo uses the `async_trait` crate to allow async functions in traits with dynamic dispatch.

If your trait needs to do dynamic dispatch (e.g. for provisioning hooks), you must use `async_trait`. Else use normal async functions.

## Rust Hypervisor Firmware failing to boot newer images

out of scope for odorobo, tracking issue [here](https://github.com/cloud-hypervisor/rust-hypervisor-firmware/issues/412)

## creating kameo handlers

You need to impl a `Message<RequestMessageType>` trait for the actor to be able to handle a message on an actor. The following is a template for this

You also need to implement 

```rs
#[remote_message]
impl Message<RequestMessageType> for Actor {
    type Reply = ReplyType;

    async fn handle(&mut self, msg: RequestMessageType, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        // this code will run whenever someone sends the actor this type of message.
        ReplyType {}
    }
}
```