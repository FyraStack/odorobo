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
- **CLI**: Debugging and management command-line interface for interacting with the platform, not intended for end-users but useful for developers and operators.

The Gateway orchestrates across nodes;
Agents run on each node and manage Cloud Hypervisor instances directly via systemd.

## Usage

Odorobo Agent is meant to be run as a system agent on each bare-metal node (or a VM with nested virtualization support) that will host VMs. The agent manages the lifecycle of Cloud Hypervisor instances on the node it runs on.

Build the Agent binary with `cargo build --release` and run it on the host machine. The Agent will listen for commands from the Gateway to create, manage, and delete VMs.

Install the systemd integration first:
```bash
# Create the `odorobo` user (the unit is hardcoded to run as this for now)
sudo useradd -r -s /usr/sbin/nologin odorobo
# Install unit hook scripts
sudo just install_script
# finally, install the unit
sudo just install_unit
```

Then build and run the agent:
```bash
# Build the Agent
cargo build --release
# Run the Agent (requires write permissions to /run/odorobo, and access to systemd's system session bus
# preferrably as the same user as Cloud Hypervisor, see `systemd/odorobo-ch@.service`
# for the CH service template)
sudo ./target/release/odorobo-agent
```

For debugging and/or small-scale single-node usage, the CLI is available to interact directly with the agent.

Install the CLI helper

```bash
cargo install --path odoroboctl
```

You can then use `odoroboctl` to directly interact with the Agent, for example to spawn a VM instance

```bash
odoroboctl spawn my-vm
```

Now apply the [Cloud Hypervisor VM spec](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/api.md#create-a-virtual-machine) to the instance, for example with a simple configuration that boots from a disk image

```bash
# the `--boot` flag additionally also tells Cloud Hypervisor to boot the VM after applying the configuration, otherwise it will stay
# in the "Created" state, requiring a separate `odoroboctl boot` call to start it.
odoroboctl create my-vm --boot ./my-vm.json
```

Now the VM should be running. You can connect to the VM's virtio-console with:

```bash
screen /run/odorobo/my-vm/console.sock
```

There is also a WebSocket proxy that can be used to connect to the console over WebSockets, for example with `websocat`.
See [docs/console.md](docs/console.md) for PTY-over-WebSocket usage and integration details.

For more advanced usage, Odorobo Agent also exposes a passthrough route for the local Cloud Hypervisor API, allowing you to call the full Cloud Hypervisor API directly through the agent's REST API

See `docs/ch-passthrough.md` for Cloud Hypervisor API passthrough usage.
