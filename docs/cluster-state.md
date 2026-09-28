# Durable cluster state

Odorobo stores desired VM manifests and scheduler placement records in etcd under
`/odorobo/v1`.

## Key layout

- `/odorobo/v1/vm-manifests/<vmid>` — serialized provider-neutral `VmManifest` records.
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
Odorobo exits if the connection or an initial status request fails. It does not
fall back to process-local writable state because doing so would let nodes accept
changes that other nodes cannot observe and that disappear on restart.

During a temporary operation failure, local VM actors and caches are not deleted.
Create persists the manifest and placement before dispatching to an agent; if
either write fails, creation is rejected. Deletes wait for the VM actor to
confirm deletion before removing the placement and manifest records. A failed
delete intentionally leaves the durable record so it can be reconciled rather
than losing desired state. Reads that fail during startup prevent the affected
actor from starting.

The storage trait and `MemoryStateStore` provide isolated tests without requiring
an etcd service. The etcd implementation uses the same versioned serialization,
prefix listing, read, write, delete, and health interfaces.
