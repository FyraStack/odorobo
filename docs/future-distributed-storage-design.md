# Future distributed storage investigation

**Status:** Planning note only. This document records options and questions for a future issue; it does not change the current container-rootfs implementation spec and does not commit Odorobo to a bespoke distributed storage system.

The current design keeps OCI blobs and per-layer composefs caches node-local. Persistent writable rootfs state can use either node-local storage (development/testing) or a per-VM Ceph RBD volume (cross-node cold restart). This investigation is about whether, and how, the data plane should evolve if Ceph performance remains inadequate.

## Problem to solve

Ceph has shown performance concerns, but the affected workload, bottleneck, and required durability guarantee need to be measured and specified before choosing a replacement. Immutable image reads, active guest writes, and cold-restart/checkpoint data have different access patterns and should not automatically share one storage backend.

Key questions:

1. Is the performance issue on reads, writes, fsync/latency, metadata operations, startup, or drain/restore time?
2. Must writes survive only a planned maintenance drain, or also sudden node loss?
3. What write-loss window (RPO), restart time (RTO), VM upper size, and number of concurrent drains are acceptable?
4. Is the desired scope only VM persistent rootfs state, or also block volumes, OCI/image cache, and other VM data?

## Storage roles

Keep three roles conceptually separate:

- **Node cache:** fast local cache for immutable OCI blobs and composefs layer objects. Cache misses can be fetched/rebuilt; it is not the only durable copy.
- **Immutable remote object/blob store:** optional source of OCI blobs or checkpoint objects, addressed by digest and verified locally before use. It is not a live POSIX rootfs.
- **Mutable per-VM state:** the persistent rootfs upper/work or a per-VM volume. It needs a single-writer ownership model and a well-defined crash/recovery contract.

A future implementation should put APIs around these roles (e.g. local cache, immutable blob store, persistent-volume attach/detach, checkpoint export/restore) rather than spreading Ceph-specific commands through the VM/rootfs logic.

## Options

| Option | Active VM writes | Planned cross-node drain | Sudden node loss | Main trade-offs |
|---|---|---|---|---|
| **Ceph RBD volume per VM** | Directly to RBD-backed filesystem/volume | Detach on source, attach on destination, cold restart | Depends on Ceph durability and filesystem recovery | Straightforward persistent-volume model; Ceph latency remains on the active write path; requires strict single-writer fencing and volume lifecycle/size policy. |
| **Node-local upper; copy/checkpoint during clean drain** | Local filesystem | Stop VM, snapshot/export upper, transfer, restore on destination | Not protected before the latest completed checkpoint | Fast active writes; planned maintenance is possible; drain can take a long time and must preserve xattrs, whiteouts, hardlinks, ownership, and filesystem state. |
| **Node-local upper; asynchronous replication/checkpointing** | Mostly local, with background transfer | Yes, after destination has a current checkpoint | Data since last successful replication may be lost | Lower foreground latency with a stated RPO; replication, backpressure, checkpoint consistency, and fencing become system responsibilities. |
| **CephFS for persistent upper/work or shared CAS** | Direct remote filesystem operations | Shared paths can be mounted at destination | Depends on CephFS durability | Avoids explicit RBD attach for directory state, but OverlayFS upper/work support, metadata latency, and fs-verity capability must be tested on the deployed CephFS/kernel combination. |
| **Ceph Object Gateway / S3-like store for blobs or checkpoints** | Not suitable as a live OverlayFS upper | Upload/download immutable checkpoints for cold restart | Only as recent as completed uploads | Useful for immutable OCI blobs and versioned checkpoints; requires packaging/chunking and restore, not arbitrary POSIX writes. Do not mount it via FUSE and treat it as local ext4. |
| **Custom local-first replicated storage** | Local NVMe with application-managed replication | Designed around Odorobo's drain protocol | Only as strong as replication/consistency design | Maximum control over latency and layout, but requires a substantial new system: chunking, logs/snapshots, checksums, replication, fencing, recovery, garbage collection, and observability. |

RBD and object storage are complementary rather than interchangeable: RBD is a mutable block volume; object storage is better for immutable blobs and checkpoint generations. A design using both can keep image data local and use RBD for active persistent state, with object storage as an optional checkpoint/blob layer.

## Candidate evolution paths

### Path A: Keep RBD for mutable persistent state

Keep the current proposed RBD backend and optimize its pool, client, volume layout, caching, and workload placement based on measurements. This is the simplest path if the measured write latency meets the product target.

### Path B: Local-first writes, checkpoint to remote storage

Keep each VM's active upper on local NVMe. At orderly stop/drain, quiesce the guest, capture a consistent checkpoint, transfer it to a destination or immutable object store, and only then permit source maintenance. This can reduce active write latency, but planned drain is a copy/restore operation, and unplanned source loss can lose uncheckpointed writes.

For correct upper transfer, preserve OverlayFS metadata (including xattrs/whiteouts), numeric ownership, hardlinks, sparse files, and filesystem state. Prefer a versioned, checksummed checkpoint format over an unqualified recursive copy. Define cleanup/retention and interrupted-transfer behavior.

### Path C: Build a storage subsystem

Only pursue this after benchmarks show current storage is a material bottleneck and the required RPO/RTO is explicit. A plausible direction is local NVMe for the hot path plus content-addressed immutable chunks/snapshots in a replicated object layer. This is not a small extension to rootfs caching: it is a distributed state system with durability, consistency, fencing, recovery, capacity management, and operational support requirements.

## Ceph-specific validation

- The local development stack currently provisions a single-node Ceph cluster with an RBD image; it does not provision CephFS/MDS or Ceph Object Gateway.
- RBD experiments can validate mapping, filesystem creation, host OverlayFS behavior, stop/detach/reattach, and data persistence. A single-node development setup cannot validate cross-host fencing or failover.
- Do not assume CephFS supports fs-verity for composefs image/object backing or is suitable as an OverlayFS upper. Probe fs-verity and run real OverlayFS upper/work tests on the deployed CephFS and kernel versions before choosing it.
- Do not put composefs on an object-store FUSE mount. Materialize and verify objects onto a filesystem whose fs-verity and composefs behavior has been validated.

## Benchmark and decision plan

1. Establish reproducible representative VM workloads: image cold/warm boot, file-heavy startup, package install/update, small random writes, large sequential writes, metadata-heavy create/delete, and fsync-heavy behavior.
2. Compare local ext4/btrfs, current RBD-backed ext4, and (if provisioned) CephFS. Record throughput plus p50/p95/p99 latency, fsync latency, startup time, and drain/restore duration.
3. Measure immutable cache behavior separately from writable upper behavior; do not attribute cache misses or registry latency to Ceph writes.
4. Exercise clean shutdown, source detach, destination attach, base-image digest validation, restart, explicit VM delete, and interrupted drain/checkpoint recovery.
5. Set explicit service targets for RPO, RTO, maximum persistent upper size, drain throughput, and node-loss behavior.
6. Select RBD, local-plus-checkpoint, CephFS, or a custom subsystem based on those results. Keep local cache as a cache unless there is a separate reason to centralize it.

## Likely follow-up issue shape

A useful future issue should identify one decision and acceptance test, rather than say “move everything to Ceph” or “build distributed storage.” For example:

- benchmark rootfs persistent-write performance and define the required RPO/RTO; or
- prototype local-upper checkpoint/restore for planned drains; or
- validate CephFS OverlayFS/fs-verity compatibility for a named Ceph/kernel version.

Keep OCI cache placement independent unless the benchmarks show it is part of the problem.
