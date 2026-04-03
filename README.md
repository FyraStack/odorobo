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

Now the VM should be running. You can connect to the VM's serial console via the agent's WebSocket proxy:

```bash
websocat --binary ws://127.0.0.1:8890/vms/my-vm/console
```

To connect directly on the host, look up the PTY path from the VM info:

```bash
odoroboctl info my-vm  # find config.serial.path, e.g. /dev/pts/3
screen /dev/pts/3
```

See [docs/console.md](docs/console.md) for WebSocket console usage and integration details.

For more advanced usage, Odorobo Agent also exposes a passthrough route for the local Cloud Hypervisor API, allowing you to call the full Cloud Hypervisor API directly through the agent's REST API

See `docs/ch-passthrough.md` for Cloud Hypervisor API passthrough usage.

## Live Migration

To start a live migration, you will first need to spawn a VM on the destination node with `odoroboctl spawn`, then call the `migrate` command on the source node with the destination node's address:

```bash
odoroboctl spawn my-vm-dest
```

Now, on the same destination VM, start accepting migrations:

```bash
odoroboctl migrate-receive my-vm-dest
```

You will now receive a response with the listening address and port for the destination machine. Use this information to start the migration from the source node.

```json
{"listening_address":"tcp:0.0.0.0:49152"}
```

Replace `0.0.0.0` with the actual IP address of the destination node, and use the provided port (e.g. `49152`)

Finally, start migration from the source node:

```bash
odoroboctl migrate-send my-vm-source tcp:<DEST-IP>:49152
```

This will start the live migration process. The source VM will continue running until the final switchover phase, at which point it will be paused, the remaining state will be transferred to the destination, and then the destination VM will be resumed.

You will have to manually manage networking and storage for the VM during migration, as Odorobo does not currently have any built-in network or storage management features. Migrations may fail if the networking configuration is unmigratable.

This part is currently out-of-scope for Odorobo, as the orchestrator should be responsible for coordinating with the network and storage layers to ensure that the VM's resources are available on the destination node before starting migration.

Odorobo however has support for custom pre and post-migration hooks, which can be used to implement custom logic before and after migration, such as preparing the destination node's network and storage configuration, or cleaning up the source node after migration. See `odorobo-agent/src/state/provisioning/hooks` for more details on how to implement and configure lifecycle hooks.

## Security notes

Currently, the `odorobo-ch@.service` unit is configured to be sandboxed and confined to a list of read-writable paths that are necessary for operation, and by default only has access to `/var/lib/odorobo` and `/dev` for runtime data.

To allow Cloud Hypervisor to access the disk, you will have to either move your disk images into `/var/lib/odorobo` or add additional read access to the paths where your disk images are stored.
