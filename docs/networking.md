# Networking Integration

Odorobo adds a networking integration layer on top of Cloud Hypervisor's network device support, using config transforms to convert Odorobo-local network references into concrete TAP device names that Cloud Hypervisor can use directly.
This allows users to specify guest network attachments in a stable, Odorobo-aware way, while letting the agent derive deterministic host-side interface names instead of relying on Cloud Hypervisor to auto-generate TAP names at runtime.

Currently, Odorobo's networking transform supports the following backend:

- **Odorobo-managed TAP naming**: Specify a network attachment by setting the network device `id` to the `net://` URI scheme, e.g. `net://devnet`. Odorobo will detect the `net://` scheme during config transformation, validate the network identifier, and rewrite the Cloud Hypervisor `NetConfig` to use a deterministic TAP device name derived from the VM ID and the network ID.

To use the networking transformer, specify the desired `net://` URI in the `id` field of a network device in your VM config. For example:

```json
{
  ...
  "net": [
    {
      "id": "net://devnet",
      "mac": "46:59:52:67:67:67"
    }
  ]
}
```

Odorobo will automatically detect the `net://` scheme and transform it before VM creation into a concrete Cloud Hypervisor network device definition. For a VM with ID `01KPB9AMVWXC2D1PF4W6E7MHWT`, the transformed config would look like:

```json
{
  ...
  "net": [
    {
      "id": "devnet",
      "tap": "vmtap-7mhwt-vnet",
      "mac": "46:59:52:67:67:67"
    }
  ]
}
```

The transformed `tap` field gives Cloud Hypervisor a known host-side TAP name to use, and the transformed `id` field removes the Odorobo-local `net://` prefix so the runtime configuration contains only the concrete network identifier.

The TAP name is deterministic per `(vmid, network_id)` pair and is generated using the following rules:

- The TAP name format is `vmtap-<vmid-suffix>-<network-id-suffix>`.
- Linux interface names are limited in length, so Odorobo truncates both the VM ID and network ID components to fit within the interface name limit.
- Odorobo keeps the **suffix** of the VM ID rather than the prefix, because ULID prefixes are timestamp-derived and less useful for uniqueness when many VMs are created close together.
- The network ID is sanitized to a conservative Linux-interface-safe character set.
- Network IDs may only contain ASCII alphanumeric characters, `_`, and `-`.

This transform currently only rewrites the VM configuration.
It does not yet imply more advanced network orchestration semantics such as routed IPv6 guest networking, dynamic TAP discovery from Cloud Hypervisor, or arbitrary external network backend resolution.

## Network modes

Odorobo's agent-side network configuration currently supports two host networking modes:

- **Host-only NAT**: A private guest bridge with a host-side gateway IP and outbound NAT through a configured upstream interface.
- **Bridged**: A flat bridge mode where the agent ensures the bridge exists and has the configured host address, but the operator is responsible for attaching any physical uplink to the bridge.

In Host-only NAT mode, the bridge itself carries the guest gateway IP on the private subnet, and guest TAP devices are attached to that bridge.
Outbound NAT is currently implemented as an IPv4-only masquerade rule in a dedicated Odorobo-owned nftables table.

> [!NOTE]
> Host-only NAT is intentionally IPv4-only for now.
>
> Although nftables can match interfaces in dual-stack tables, Odorobo's current host-only networking model is explicitly built around IPv4 guest subnet and gateway configuration.
> Enabling IPv6 NAT would be a larger policy decision, because it would implicitly opt Odorobo into an IPv6 guest-networking story and NAT66 behavior.
> Until Odorobo has intentional IPv6 guest-network design, host-only NAT remains scoped to IPv4.

## DHCP integration

If DHCP is configured in the agent's networking config, Odorobo starts `dnsmasq` bound to the configured guest bridge and serves leases for the configured subnet and range.
This is intended for host-only private guest networks where Odorobo owns the bridge and guest-side address distribution.

## Debug NAT networking

Host-only NAT is the default mode and is meant for development and debugging on a single machine. It works like libvirt's default network: a private bridge on the host, `dnsmasq` handing out addresses, and masquerade NAT out through the host's uplink. Guests can reach the host and the outside world; nothing outside can initiate connections to guests.

### What the agent sets up

On startup the network actor does the following, in order:

1. Creates the bridge (`vmbr0` by default) if it does not exist and brings it up.
2. Adds the gateway address (for example `10.10.100.1/24`) to the bridge. Other addresses already on the bridge are left alone.
3. Creates an nftables table `ip odorobo` with a `guest_nat` postrouting chain and a rule `oifname "<upstream>" masquerade`. The rule matches all traffic leaving the upstream interface, not only traffic from the guest subnet.
4. Starts `dnsmasq --interface <bridge> --bind-interfaces --dhcp-range ... --no-daemon` as a child process when `dhcp_config` is set. `dnsmasq` also serves DNS on the gateway address, and its DHCP log lines appear in the agent's journal.

When a VM boots, the transform names its TAP device `vmtap-<vmid suffix>-<network suffix>`. Cloud Hypervisor creates the TAP, then the after-boot hook enslaves it to the bridge and brings it up. The before-stop hook detaches it.

The agent does **not** enable `net.ipv4.ip_forward`, and it adds no forwarding or input firewall rules. The host must forward IPv4 itself (`sysctl -w net.ipv4.ip_forward=1`), and any host firewall must accept traffic from the bridge.

### Configuration

The agent reads `config.json` from its **working directory**, not from the repository path or `/etc`. A missing file is only a warning, and the agent then runs with the defaults. The defaults put the gateway at `10.0.0.1`, name the bridge `vmbr0`, and have **no `dhcp_config`**, so guests get no addresses.

```json
{
  "network": {
    "dhcp_config": {
      "range": ["10.10.100.100", "10.10.100.200"],
      "subnet": "10.10.100.0/24",
      "lease_time": "12h"
    },
    "network_mode": {
      "HostonlyNat": {
        "bridge": "vmbr0",
        "subnet": "10.10.100.0/24",
        "gateway": "10.10.100.1"
      }
    }
  }
}
```

`upstream_iface` is optional. When omitted, the agent uses the interface of the first `default` route reported by `ip route`, falling back to `eth0`. `dnsmasq` must be installed on the host. The repository's `odorobo/config.json` is a working example but also sets a Sentry DSN, so use a copy without `sentry_dsn` for local runs.

### Attaching a VM

Add the network to the manifest. The ID becomes the TAP name suffix and may contain only ASCII letters, digits, `-` and `_`.

```json
"networks": [{ "id": "devnet", "mac_address": "46:59:52:67:67:67" }]
```

Omit `mac_address` unless you need a fixed address. Two VMs on the same bridge with the same MAC receive the same lease and conflict.

### Guest configuration

The agent configures only the host side. Something inside the guest must bring `eth0` up and run a DHCP client, which most images do through their OCI `ENTRYPOINT`/`CMD`. A bare shell as PID 1 does neither, and the Fedora container image ships no `ip` or DHCP client. For a quick test, let the kernel do it by adding `ip=dhcp` to `boot.cmdline`, which makes the guest configure `eth0` before init runs:

```json
"boot": { "start": true, "cmdline": "console=ttyS0 ip=dhcp" }
```

The console then shows `IP-Config: Got DHCP answer from 10.10.100.1`, with the gateway and nameserver set to the bridge address. Images with their own network management do not need `ip=dhcp`.

### Inspecting and debugging

```bash
ip -br addr show vmbr0                  # gateway address
bridge link | grep vmbr0                # attached TAP devices
sudo nft list table ip odorobo          # masquerade rule
pgrep -a dnsmasq                        # DHCP server and its range
sudo journalctl -u <agent unit> | grep dnsmasq-dhcp   # DISCOVER/OFFER/REQUEST/ACK
```

If a guest gets no lease, check the bridge first. `dnsmasq` running with the wrong range means the agent started without your `config.json`. If a guest has an address but no connectivity, check `ip_forward` and the host firewall.

### Cleanup

The bridge, its addresses and the nftables table are not removed when the agent stops. To reset them:

```bash
sudo ip link del vmbr0
sudo nft delete table ip odorobo
```

## Limitations

At the moment, the `net://` transformer is intentionally conservative:

- It only interprets `net://<network_id>` in the network device `id` field.
- It does not currently resolve arbitrary remote or provider-backed virtual networks.
- It does not currently create a higher-level virtual network inventory or multi-tenant network object model.
- It relies on deterministic TAP naming instead of Cloud Hypervisor runtime TAP discovery, because Cloud Hypervisor does not reliably reflect auto-generated TAP device names back into VM info after boot.

This makes the current networking integration a practical first step: Odorobo can transform user-friendly network references into stable Cloud Hypervisor network config while keeping host-side naming deterministic and debuggable.