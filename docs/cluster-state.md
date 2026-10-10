# Durable cluster state

Odorobo stores desired VM manifests and scheduler placement records in etcd under
`/odorobo/v1`.

## Key layout

- `/odorobo/v1/vm-manifests/<vmid>` — serialized provider-neutral `VmManifest` records.
- `/odorobo/v1/placement/<vmid>` — selected node, create generation, and lifecycle.
- `/odorobo/v1/node-state/<node>` — reserved for node-state records.
- `/odorobo/v1/operations/<operation>` — reserved for operation records.

Every value is wrapped in a record containing a numeric `version` and `value`.
Readers reject unsupported versions with `UnsupportedVersion`; records are not
silently interpreted using a newer or older schema. Placement lifecycle and
create-generation fields default when reading older version-1 records, so they
remain active with a nil legacy generation token. A future schema migration should
read the old version, transform it explicitly, and write the current version.

The placement lifecycle is `active`, `stopping`, or `deleting`. The scheduler
writes a stop marker before asking the owner agent to tear down the VM. The
manifest and placement remain durable while teardown is unconfirmed; only after
the owner confirms stop are both records removed. Startup and reconciliation
finish marked stops instead of recreating them. A generation token fences
previously dispatched creates and stop commands from later VM incarnations. Stop
requests and acknowledgements carry the expected generation, owner, and lifecycle;
finalization compare-and-swaps that exact placement before removing either key.
A delayed acknowledgement for an old generation therefore cannot remove a new
create, even if the new incarnation is already stopping.

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
Create persists the manifest and placement before dispatching to an agent. If a
write response is ambiguous, the manager rereads both records and adopts the
original manifest and owner only when they match the request. Agents validate
every create against current etcd intent and reject stale generations, stopped
VMs, and commands for a different owner. They revalidate after startup and before
publishing the actor or charging it as running. A failed or uncertain teardown
keeps the VM's resources reserved and prevents reuse of its runtime path. Managers
refresh paired manifest/placement state from one etcd snapshot (or one memory
read lock); a torn or orphaned manifest is not an active create. Managers refresh
durable intent rather than treating process-local maps as authority. A durable
placement is not reassigned merely because its owner is temporarily unreachable;
cross-host automatic reassignment requires runtime fencing that is not currently
provided.

Shutdown and delete both persist a stop intent first. They wait for the owning
agent to confirm runtime teardown before finalizing/removing the manifest and
placement. Failed or ambiguous teardown leaves the marker and records in place
for recovery rather than losing track of a possibly running VM. Reads that fail
during startup prevent the affected actor from starting.

The storage trait and `MemoryStateStore` provide isolated tests without requiring
an etcd service. The etcd implementation uses the same versioned serialization,
prefix listing, read, write, delete, and health interfaces.
