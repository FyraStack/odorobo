# Future Work

This document records durable design work that is intentionally outside the
current implementation. It is not a statement that these capabilities are
available today.

## Scheduling Policy

- Admission control currently reserves each VM's requested boot vCPUs. Decide
  whether a VM's `max_vcpus` must also be reserved before CPU hotplug is
  exposed as a supported operation.
- RAM is not overcommitted. Any RAM overcommit policy depends on verified
  Cloud Hypervisor behavior and must include host-level safeguards.
- Consider reserving a configurable fraction of otherwise empty agents for
  conversion to dedicated hosts. Such agents should remain eligible only when
  no ordinary capacity is available.

## VM Intent And High Availability

- Persist or rebuild VM intent, placement, and agent membership after
  scheduler failover. A replacement scheduler must reconcile each agent's VM
  inventory before scheduling new work.
- Allocate and publish a VM's MAC address as part of VM intent creation. The
  network or router update may be asynchronous, but it must not race VM
  creation or produce an unstable address.

## Migration

- Define post-migration cleanup for source runtime state, destination
  reservations, and failed or interrupted migrations.
- Track active migrations at a scheduler-wide level so recovery can identify
  the source, destination, and current phase after a process restart.
- Verify and fix vsock behavior during live migration before treating the two
  features as compatible.

## Networking

IPv6 guest networking is intentionally not part of host-only NAT yet. See
[Networking Integration](networking.md#network-modes) for the current IPv4
scope and design constraints.
