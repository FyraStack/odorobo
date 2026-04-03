# Console API

## PTY over WebSocket

The agent exposes a VM console bridge at:

`GET /vms/{vmid}/console`

This endpoint upgrades to a WebSocket and proxies the VM's serial console PTY in both directions. Cloud Hypervisor allocates a PTY for the serial console; the agent looks up the PTY path via the CH API and opens it on behalf of the client.

## Connection behavior

- Connect with a standard WebSocket client to `ws://<agent-host>:8890/vms/<vmid>/console`
- The upgrade succeeds only if the VM exists and the agent can open its console PTY
- If the VM does not exist, the HTTP request returns `404`
- If the PTY cannot be opened, the HTTP request returns `500`
- After the upgrade completes, terminal bytes flow over the socket until either side disconnects

## Frame semantics

- Client -> VM:
  - WebSocket `Binary` frames are written to the PTY as-is
  - WebSocket `Text` frames are reserved for JSON control messages
- VM -> Client:
  - PTY output is sent back as WebSocket `Binary` frames
  - Server control or protocol errors are sent as WebSocket `Text` frames containing JSON
- Control frames:
  - WebSocket `Ping` receives a `Pong`
  - WebSocket `Close` closes the session

In practice, treat this as a raw byte stream carried over WebSocket frames.

## Resize message

Clients resize the PTY by sending a JSON text frame like:

```json
{"type":"resize","cols":120,"rows":40}
```

Optional pixel dimensions are also supported:

```json
{"type":"resize","cols":120,"rows":40,"x_pixels":960,"y_pixels":720}
```

The agent applies the new TTY window size on the host PTY with `TIOCSWINSZ`, so the guest can observe the updated size through normal terminal mechanisms.

## Reset-session message

Clients can ask the agent to try to return the console to a fresh login-like state by sending:

```json
{"type":"reset_session"}
```

This sends a conservative control sequence to the PTY:

- `Ctrl-C` to interrupt a foreground shell command if possible
- `Enter` to try to land on a clean prompt
- `Ctrl-D` to request EOF/logout from the current shell or login program

This is best-effort only. The guest decides what those bytes mean.

## Important implementation notes

- Do not assume one terminal message maps to one WebSocket frame; PTY output is chunked arbitrarily
- Send terminal input as binary bytes, not text frames
- Text frames should be treated as a small control channel for messages like resize requests, reset requests, and error events
- If the server receives an invalid control message, it responds with a JSON text frame like `{"type":"error","message":"..."}` and keeps the session open
- This API is transport-only; terminal emulation, ANSI parsing, scrollback, and rendering are client responsibilities
- `reset_session` is heuristic: it works best when the guest runs a normal login shell or `agetty` on `hvc0`
- On serial-backed Linux sessions, `Ctrl-D` only causes a logout when the foreground process interprets it as EOF
- If the guest is running `vim`, `less`, a full-screen app, or a raw-mode program, `reset_session` may not produce a fresh login prompt
- For deterministic fresh sessions, configure the guest to respawn `getty` on the console device after shell exit
- The PTY path changes if the VM is restarted or migrated; the WebSocket connection must be re-established after migration

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

## CLI example with websocat

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

This is the simplest way to verify the bridge works before integrating it into a browser terminal such as xterm.js.
