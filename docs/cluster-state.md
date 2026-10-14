# Durable cluster state

Odorobo stores desired VM manifests and manager-owned placements in etcd under
`/odorobo/v1`. Records are JSON wrappers with a numeric `version`; readers reject
unsupported versions instead of interpreting them as a known schema.

## Keys and configuration

- `/odorobo/v1/vm-manifests/<vmid>` — provider-neutral `VmManifest` intent.
- `/odorobo/v1/placement/<vmid>` — owning agent hostname, create generation, and
  lifecycle (`active`, `stopping`, or `deleting`). Older version-1 records
  default to active with no generation.
- `/odorobo/v1/node-state/<node>` stores a version-1 `NodeStateRecord` payload:
  `{"node":"node-a","metadata":{"labels":{},"annotations":{}}}`. The node
  field matches the key suffix; metadata uses the shared labels/annotations shape.
- `/odorobo/v1/operations/<operation_id>` stores a version-1 `OperationRecord`
  payload with `operation_id` (ULID), opaque `kind` and `target` strings, and a
  `state` of `pending`, `running`, `succeeded`, or `failed`. These are serde
  payload contracts only: no operation processing, state transitions, or
  runtime orchestration is implemented.

Configuration is available through CLI, environment, or `config.json`:

- `etcd_endpoints` — comma-separated endpoints; defaults to
  `http://127.0.0.1:2379`.
- `etcd_username` and `ODOROBO_ETCD_PASSWORD` — optional authentication.
- `etcd_tls` and `etcd_ca_file` — enable TLS and select the CA PEM file.
- `etcd_timeout_ms` — request/connect timeout, default `5000`.
- `etcd_retries` — connection attempts, default `3`.

Passwords are not logged. Startup fails if etcd is unavailable or the initial
status request fails; Odorobo does not fall back to writable process-local state.
Health is exposed through the state-store API.

## Mutation and outage behavior

Each single-VM operation reads only that VM's manifest and placement, in one
etcd transaction (or one memory-store lock). Listing reads both prefixes at one
revision. An orphaned, mismatched, or undecodable pair is reported for that VM
only: scheduler startup logs and skips it, and other VMs remain manageable.
Operations on the damaged VM itself fail and leave the records for
investigation.

A create writes both records atomically before dispatch. Retrying the same
active manifest and placement is idempotent; conflicting, incomplete, or
stopping intent is rejected. A timed-out or ambiguous write/dispatch is not
destructively compensated. An agent checks the exact manifest, owner,
generation, and active lifecycle before using a cached actor or starting a
runtime. A runtime that fails to start releases its claimed runtime directory,
so the create can be retried.

Agent startup does not spawn VMs from etcd. Scheduler startup loads paired
intent, while actor discovery remains observational: it does not recreate a VM
or assign an unplaced manifest. A durable placement is not reassigned merely
because its owner is unreachable.

Shutdown and delete first persist a stop marker, then contact only the recorded
owner, which validates the marker before tearing anything down:

- With a live cached VM actor, the actor tears the runtime down.
- Otherwise (agent restart, failed start, dead actor, or an earlier timed-out
  teardown), the agent inspects the VM's runtime directory. Spawn claims that
  directory exclusively and only a completed cleanup removes it, so its absence
  confirms teardown. If it exists, the VMM is shut down through its API, or,
  when the API is unreachable, cleaned up once the VMM is confirmed gone: no
  process is listening on its socket. A socket that accepts connections but does
  not answer is not confirmation. Teardown releases resources using the
  transformed VM config saved in the runtime directory.
- A VMM process owned by the agent that stops answering its API is killed and
  reaped before cleanup.

A missing owner, an unconfirmed teardown, a timeout, or a teardown error leaves
the marker and both records in place; retrying the same request is safe. Only a
confirmed teardown allows an exact owner/generation/lifecycle compare-and-delete
of the pair. Finalizing an already-absent pair is idempotent, while an
incomplete pair is rejected and retained for investigation. An etcd error
during that final transaction can have an ambiguous outcome; retry rather than
compensating destructively. If both VM records are absent, delete and shutdown
return success without dispatch.

Shutdown and delete currently finalize identically: there is no stopped-VM
record, so a shut-down VM is removed from durable state and is started again
with a new create.

The scheduler waits up to 60 seconds for a create or stop reply and the agent
bounds each teardown at 45 seconds, so the agent's own timeout error reaches
the caller. These waits run off the scheduler's message loop.

A pre-existing runtime directory still makes create fail rather than adopt the
runtime; delete it first. Crash recovery, automatic actor recreation, and
runtime reattachment are follow-up work (issue 101).

The etcd transactions fence paired persistence operations, not concurrent
scheduler decisions. Operate with one active mutation manager; this is not a
complete active-active scheduling protocol. `MemoryStateStore` exercises the
same versioned records, paired-state checks, and stop fences without etcd.

## Testing against etcd

The paired-state tests run against memory and, when
`ODOROBO_TEST_ETCD_ENDPOINTS` is set, against a real etcd too. They use fresh
VM IDs and remove their keys, so a development etcd is safe to use:

```bash
ODOROBO_TEST_ETCD_ENDPOINTS=http://127.0.0.1:2379 cargo test -p odorobo cluster_state
```
