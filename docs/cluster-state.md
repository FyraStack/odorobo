# Durable cluster state

Odorobo stores desired VM manifests and scheduler placement records in etcd under
`/odorobo/v1`.

## Key layout

- `/odorobo/v1/vm-manifests/<vmid>` — serialized `VirtualMachine` manifests.
- `/odorobo/v1/placement/<vmid>` — the selected node for a VM.
- `/odorobo/v1/node-state/<node>` — reserved for node-state records.
- `/odorobo/v1/operations/<operation>` — reserved for operation records.

Every value is wrapped in a record containing a numeric `version` and `value`.
Readers reject unsupported versions with `UnsupportedVersion`; records are not
silently interpreted using a newer or older schema. A future migration should
read the old version, transform it explicitly, and write the current version.

## Configuration

The following CLI/configuration fields are available:

- `etcd_endpoints` — comma-separated endpoints; defaults to
  `http://127.0.0.1:2379`.
- `etcd_username` and `ODOROBO_ETCD_PASSWORD` — optional authentication.
- `etcd_tls` and `etcd_ca_file` — enable TLS and select the CA PEM file.
- `etcd_timeout_ms` — request/connect timeout, default `5000`.
- `etcd_retries` — connection attempts, default `3`.

Passwords are never included in startup configuration logs.

## Availability behavior

Startup attempts to connect to etcd with the configured timeout and retry count.
If the connection cannot be established, Odorobo logs an error and uses an
in-memory store so the local process can continue operating. That fallback is
**not durable**; the health log reports the active backend. Operators should
restore etcd and restart the process to recover durable state.

During a temporary operation failure, local VM actors and caches are not deleted.
A failed manifest or placement write is logged, and a failed delete intentionally
leaves the durable record so it can be reconciled rather than losing desired
state. Reads that fail during startup leave the local cache empty and do not
perform destructive cleanup.

The storage trait and `MemoryStateStore` provide isolated tests without requiring
an etcd service. The etcd implementation uses the same versioned serialization,
prefix listing, read, write, delete, and health interfaces.
