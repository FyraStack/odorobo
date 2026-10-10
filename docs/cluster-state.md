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

Manifest and placement reads use one etcd range snapshot (or one memory-store
read lock); orphaned pairs are rejected. A create writes both records atomically
before dispatch. Retrying the same active manifest and placement is idempotent;
conflicting, incomplete, or stopping intent is rejected. A timed-out or
ambiguous write/dispatch is not destructively compensated. An agent checks the
exact manifest, owner, generation, and active lifecycle before using a cached
actor or starting a runtime.

Agent startup does not spawn VMs from etcd. Scheduler startup loads paired
intent, while actor discovery remains observational: it does not recreate a VM
or assign an unplaced manifest. A durable placement is not reassigned merely
because its owner is unreachable.

Shutdown and delete first persist a stop marker, then contact only the recorded
owner. The owner must have the VM actor cached and confirm teardown; an empty
cache alone is not proof that a runtime is absent. A missing owner, unconfirmed
empty cache, timeout, or teardown error leaves the marker and both records in
place. After a successful teardown, the agent retains the exact owner/generation/
lifecycle fence in process memory so a retry can finalize even though teardown
removed the cached actor; an unrelated empty cache remains unconfirmed. This
small confirmation is lost on agent restart. Only a confirmed teardown allows
an exact owner/generation/lifecycle compare-and-delete of the pair. Finalizing
an already-absent pair is idempotent, while an incomplete pair is rejected and
retained for investigation. An etcd error during that final transaction can
have an ambiguous outcome; refresh state and retry idempotently rather than
compensating destructively. If both VM records are absent, delete and shutdown
return success without dispatch; an incomplete pair remains an error. An
unfinished stop may require an explicit retry or operator investigation,
including after manager restart.

Cloud Hypervisor runtime paths are not automatically reattached, unlinked, or
purged when their ownership is uncertain. A pre-existing runtime directory
causes create to fail safely and may need operator inspection. Crash recovery,
automatic actor recreation, and runtime reconciliation are follow-up work
(identified as issue 101); this feature does not claim crash-safe resource
release or runtime fencing.

The etcd transactions fence paired persistence operations, not concurrent
scheduler decisions. Operate with one active mutation manager; this is not a
complete active-active scheduling protocol. `MemoryStateStore` exercises the
same versioned records, paired-state checks, and stop fences without etcd.
