# Console API

## Serial console over WebSocket

The agent exposes a VM console bridge at:

`GET /vms/{vmid}/console`

This endpoint upgrades to a WebSocket and proxies the VM's serial console in both directions. The console is backed by a Unix domain socket at `/run/odorobo/vms/{vmid}/console.sock`, which Cloud Hypervisor manages directly via its serial console socket mode.

## Connection behavior

- Connect with a standard WebSocket client to `ws://<agent-host>:8890/vms/<vmid>/console`
- The upgrade succeeds only if the VM exists and the agent can connect to its console socket
- If the VM does not exist, the HTTP request returns `404`
- If the console socket cannot be opened, the HTTP request returns `500`
- After the upgrade completes, terminal bytes flow over the socket until either side disconnects

## Frame semantics

- Client -> VM:
  - WebSocket `Binary` frames are written to the console socket as-is
  - WebSocket `Text` frames are reserved for JSON control messages
- VM -> Client:
  - Console output is sent back as WebSocket `Binary` frames
  - Server control or protocol errors are sent as WebSocket `Text` frames containing JSON
- Control frames:
  - WebSocket `Ping` receives a `Pong`
  - WebSocket `Close` closes the session

In practice, treat this as a raw byte stream carried over WebSocket frames.

## Resize message

The resize control message is accepted but has no effect with socket-backed serial consoles:

```json
{"type":"resize","cols":120,"rows":40}
```

Optional pixel dimensions are also accepted:

```json
{"type":"resize","cols":120,"rows":40,"x_pixels":960,"y_pixels":720}
```

Unlike PTY-backed consoles, a Unix socket has no associated terminal window size. `TIOCSWINSZ` is not applicable here, so the guest cannot observe a resize event through this mechanism. Terminal size negotiation must be handled at the application layer if needed.

## Reset-session message

Clients can ask the agent to try to return the console to a fresh login-like state by sending:

```json
{"type":"reset_session"}
```

This sends a conservative control sequence to the console:

- `Ctrl-C` to interrupt a foreground shell command if possible
- `Enter` to try to land on a clean prompt
- `Ctrl-D` to request EOF/logout from the current shell or login program

This is best-effort only. The guest decides what those bytes mean.

## Important implementation notes

- Do not assume one terminal message maps to one WebSocket frame; console output is chunked arbitrarily
- Send terminal input as binary bytes, not text frames
- Text frames should be treated as a small control channel for messages like resize requests, reset requests, and error events
- If the server receives an invalid control message, it responds with a JSON text frame like `{"type":"error","message":"..."}` and keeps the session open
- This API is transport-only; terminal emulation, ANSI parsing, scrollback, and rendering are client responsibilities
- The `resize` message is accepted but has no effect; the console socket has no associated terminal size
- `reset_session` is heuristic: it works best when the guest runs a normal login shell or `agetty` on the serial device
- `Ctrl-D` only causes a logout when the foreground process interprets it as EOF
- If the guest is running `vim`, `less`, a full-screen app, or a raw-mode program, `reset_session` may not produce a fresh login prompt
- For deterministic fresh sessions, configure the guest to respawn `getty` on the console device after shell exit
- The console socket at `/run/odorobo/vms/{vmid}/console.sock` is managed by Cloud Hypervisor and survives live migration; the WebSocket connection will break during migration but the socket path remains stable on the destination node

## Browser example

```js
const ws = new WebSocket("ws://127.0.0.1:8890/vms/my-vm/console");
ws.binaryType = "arraybuffer";
const encoder = new TextEncoder();
const decoder = new TextDecoder();

ws.addEventListener("open", () => {
  // Send a command to the guest console.
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

## CLI examples

Connect via the WebSocket bridge with websocat:

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

Connect directly to the console socket on the host (useful for debugging):

```bash
# raw output only
socat - UNIX-CONNECT:/run/odorobo/vms/my-vm/console.sock

# with local terminal in raw mode (proper interactive session)
socat file:`tty`,raw,echo=0 UNIX-CONNECT:/run/odorobo/vms/my-vm/console.sock
```

The websocat approach works from any machine that can reach the agent. The socat approach requires local host access but bypasses the agent entirely and is useful for low-level debugging or when the agent is not running.
