# Odorobo

> [Umicha - Odorobo](https://youtu.be/D_UC0WJmLnc)

Stack Virtualization Platform - Powered by Cloud Hypervisor

Odorobo<sup>(robot dance)</sup> is a hypervisor platform built on top of 
[Cloud Hypervisor](https://www.cloudhypervisor.org/), a lightweight 
VMM built on rust-vmm, with a focus on providing lightweight,
stateful, and portable virtual machines.

VM configuration is persisted by the gateway and portable across nodes;
disk images can be backed by distributed storage for full portability, or kept node-local for simplicity.

## Components

- **Agent**: A lightweight agent that handles VM lifecycle management, including creation, deletion, and state management.
- **Gateway**: A central controller that orchestrates the deployment and management of VMs, reconciling and handling migration across nodes (unimplemented).
- **CLI**: Debugging and management command-line interface for interacting with the platform, not intended for end-users but useful for developers and operators (unimplemented).

The Gateway orchestrates across nodes;
Agents run on each node and manage Cloud Hypervisor instances directly via systemd.

## Usage

See `docs/console.md` for PTY-over-WebSocket usage and integration details.
See `docs/ch-passthrough.md` for Cloud Hypervisor API passthrough usage.
