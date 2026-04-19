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

```bash
# Install dependencies (fedora)
sudo dnf in -y clang-devel nftables

# Build the Agent
cargo build --release

# create basic config
echo "{}" >> config.json

# Run the Agent (requires write permissions to /run/odorobo, and access to systemd's system session bus
sudo ./target/release/odorobo-agent
```

You will now be required to run the manager process:
```
# Build the Manager
cargo build --release -p odorobo-manager
# Run the Manager
./target/release/odorobo-manager
```

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

To connect directly on the host, connect to the VM's serial console socket in its runtime directory:

```bash
sudo socat -,rawer UNIX-CONNECT:/run/odorobo/vms/01KPBBXKK0R0M09VN7G6R6R3JF/console.sock
```

Replace the VM ID in the socket path with your VM's ID. The serial console socket is created at:

```text
/run/odorobo/vms/<vmid>/console.sock
```

See [docs/console.md](docs/console.md) for direct serial socket access, WebSocket console usage, and integration details.

For more advanced usage, Odorobo Agent also exposes a passthrough route for the local Cloud Hypervisor API, allowing you to call the full Cloud Hypervisor API directly through the agent's REST API

See `docs/ch-passthrough.md` for Cloud Hypervisor API passthrough usage.
