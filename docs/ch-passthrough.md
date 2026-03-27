# Cloud Hypervisor Passthrough API

The agent exposes a passthrough route for the local Cloud Hypervisor API at:

`/{vmid}/ch/{*path}`

This lets clients call Cloud Hypervisor's Unix-socket HTTP API through the agent's normal HTTP listener.

## Path mapping

Requests are rewritten like this:

- Agent route: `/{vmid}/ch/vm.info`
- Cloud Hypervisor route: `/api/v1/vm.info`

The passthrough automatically prefixes the requested path with `/api/v1/` before forwarding it to the VM's `ch.sock` Unix socket.

## Behavior

- Any HTTP method is accepted: `GET`, `PUT`, `POST`, `PATCH`, `DELETE`, and so on
- Request bodies are forwarded as-is
- Query strings are preserved
- Most request headers are forwarded through unchanged
- `Host` and `Content-Length` are not forwarded directly; the HTTP client rebuilds them for the Unix-socket request
- The response status, headers, and body from Cloud Hypervisor are returned directly to the caller

## Error handling

- If the VM does not exist, the agent returns `404`
- If the agent cannot reach the VM's Cloud Hypervisor socket, the agent returns `500`
- If Cloud Hypervisor itself returns an error like `400`, `404`, or `500`, that exact response is passed back to the caller

## Examples

Fetch VM information:

```bash
curl http://127.0.0.1:8890/vms/my-vm/ch/vm.info
```

Ping the VMM:

```bash
curl http://127.0.0.1:8890/vms/my-vm/ch/vmm.ping
```

Pause a VM:

```bash
curl -X PUT http://127.0.0.1:8890/vms/my-vm/ch/vm.pause
```

Resume a VM:

```bash
curl -X PUT http://127.0.0.1:8890/vms/my-vm/ch/vm.resume
```

Resize a VM with a JSON body:

```bash
curl -X PUT \
  -H 'content-type: application/json' \
  -d '{"desired_vcpus":2,"desired_ram":1073741824}' \
  http://127.0.0.1:8890/vms/my-vm/ch/vm.resize
```

Pass through a query string:

```bash
curl "http://127.0.0.1:8890/vms/my-vm/ch/some.endpoint?foo=bar&baz=qux"
```

## Notes

- This route is intentionally low-level and mirrors Cloud Hypervisor's API fairly directly
- It is useful for debugging, advanced control flows, and reaching endpoints that do not yet have first-class agent routes
- Clients should still prefer dedicated agent routes when available, because those routes can provide better validation and more stable semantics
