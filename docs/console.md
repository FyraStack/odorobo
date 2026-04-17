# Console Integration

Odorobo configures the guest serial console as a UNIX socket owned by the agent rather than relying on a host PTY allocated by Cloud Hypervisor.
This gives each VM a stable, deterministic host-side serial endpoint under the VM runtime directory.

Currently, Odorobo's console integration supports the following access patterns:

- **Direct host-side serial socket access**: Each VM's serial console is configured as a UNIX socket at `/run/odorobo/vms/<vmid>/console.sock`. This can be connected to directly from the host with tools such as `socat`.
- **WebSocket console bridge**: The agent exposes a WebSocket endpoint at `GET /vms/{vmid}/console` and proxies the VM's serial socket in both directions for browser or CLI clients.

## Serial socket configuration

During VM config transformation, Odorobo rewrites the serial console configuration to use Cloud Hypervisor's socket-backed serial mode.
For a VM with ID `01KPBBXKK0R0M09VN7G6R6R3JF`, the serial console socket will be created at:

```text
/run/odorobo/vms/01KPBBXKK0R0M09VN7G6R6R3JF/console.sock
```

This path is stable for the lifetime of the VM runtime directory and is derived from the VM ID.

## Direct host access

To connect directly to the guest serial console on the host, connect to the socket with `socat`:

```bash
sudo socat -,rawer UNIX-CONNECT:/run/odorobo/vms/01KPBBXKK0R0M09VN7G6R6R3JF/console.sock
```

This is currently the simplest direct way to verify that the guest serial console is alive and accepting input.

A few notes about this connection style:

- `rawer` is important so local terminal processing does not interfere with the serial byte stream.
- You will typically need sufficient permissions to access the runtime directory and socket, which is why `sudo` is commonly required.
- This is a raw serial stream, not a terminal emulator. Line editing, ANSI handling, scrollback, and rendering are provided by your terminal, not by Odorobo.

## WebSocket bridge

The agent exposes a VM console bridge at:

`GET /vms/{vmid}/console`

This endpoint upgrades to a WebSocket and proxies the VM's serial console socket in both directions.

## Connection behavior

- Connect with a standard WebSocket client to `ws://<agent-host>:8890/vms/<vmid>/console`
- The upgrade succeeds only if the VM exists and the agent can open its serial console socket
- If the VM does not exist, the HTTP request returns `404`
- If the serial socket cannot be opened, the HTTP request returns `500`
- After the upgrade completes, terminal bytes flow over the socket until either side disconnects

In practice, treat this as a raw byte stream carried over WebSocket frames.

## Frame semantics

- Client -> VM:
  - WebSocket `Binary` frames are written to the serial socket as-is
  - WebSocket `Text` frames are reserved for JSON control messages
- VM -> Client:
  - A `{"type":"connected","vm_id":"..."}` text frame is sent immediately on upgrade before any console data
  - Serial console output is sent back as WebSocket `Binary` frames
  - Server control or protocol errors are sent as WebSocket `Text` frames containing JSON
- Control frames:
  - WebSocket `Ping` receives a `Pong`
  - WebSocket `Close` closes the session

## Resize message

Clients resize the console session by sending a JSON text frame like:

```json
{"type":"resize","cols":120,"rows":40}
```

Optional pixel dimensions are also supported:

```json
{"type":"resize","cols":120,"rows":40,"x_pixels":960,"y_pixels":720}
```

The agent applies the new terminal window size on the host-side console endpoint so the guest can observe the updated size through normal terminal mechanisms.

## Reset-session message

Clients can ask the agent to try to return the console to a fresh login-like state by sending:

```json
{"type":"reset_session"}
```

This sends a conservative control sequence to the serial console:

- `Ctrl-C` to interrupt a foreground shell command if possible
- `Enter` to try to land on a clean prompt
- `Ctrl-D` to request EOF/logout from the current shell or login program

This is best-effort only. The guest decides what those bytes mean.

## Important implementation notes

- Do not assume one terminal message maps to one WebSocket frame; serial output is chunked arbitrarily
- Send terminal input as binary bytes, not text frames
- Text frames should be treated as a small control channel for messages like resize requests, reset requests, and error events
- If the server receives an invalid control message, it responds with a JSON text frame like `{"type":"error","message":"..."}` and keeps the session open
- This API is transport-only; terminal emulation, ANSI parsing, scrollback, and rendering are client responsibilities
- `reset_session` is heuristic: it works best when the guest runs a normal login shell or `agetty` on the configured serial console
- On serial-backed Linux sessions, `Ctrl-D` only causes a logout when the foreground process interprets it as EOF
- If the guest is running `vim`, `less`, a full-screen app, or a raw-mode program, `reset_session` may not produce a fresh login prompt
- For deterministic fresh sessions, configure the guest to respawn `getty` on the serial console after shell exit
- The console socket path is stable for a given VM runtime directory, but the socket itself is recreated when the VM is restarted or migrated, so clients must reconnect after those events

## Browser example

```js
const ws = new WebSocket("ws://127.0.0.1:8890/vms/my-vm/console");
ws.binaryType = "arraybuffer";
const encoder = new TextEncoder();
const decoder = new TextDecoder();

ws.addEventListener("open", () => {
  ws.send(encoder.encode("uname -a\n"));
  ws.send(JSON.stringify({ type: "resize", cols: 120, rows: 40 }));
});

ws.addEventListener("message", async (event) => {
  if (typeof event.data === "string") {
    const control = JSON.parse(event.data);
    if (control.type === "error") {
      console.error(control.message);
    }
    return;
  }

  const data = event.data instanceof ArrayBuffer
    ? new Uint8Array(event.data)
    : new Uint8Array(await event.data.arrayBuffer());

  const text = decoder.decode(data);
  console.log(text);
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    ws.send(new Uint8Array([13]));
    return;
  }

  if (event.key.length === 1) {
    ws.send(encoder.encode(event.key));
  }
});

function resizeTerminal(cols, rows) {
  ws.send(JSON.stringify({ type: "resize", cols, rows }));
}

function resetSession() {
  ws.send(JSON.stringify({ type: "reset_session" }));
}
```

## CLI example with websocat

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

This is the simplest way to verify the WebSocket bridge works before integrating it into a browser terminal such as xterm.js.

## Summary

For most debugging and operator workflows:

- use `socat` when you are already on the host and want a direct raw serial connection
- use the WebSocket bridge when you want remote access or browser integration

Both paths ultimately connect to the same guest serial console socket managed by the agent.