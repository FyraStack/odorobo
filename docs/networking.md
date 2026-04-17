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

## Limitations

At the moment, the `net://` transformer is intentionally conservative:

- It only interprets `net://<network_id>` in the network device `id` field.
- It does not currently resolve arbitrary remote or provider-backed virtual networks.
- It does not currently create a higher-level virtual network inventory or multi-tenant network object model.
- It relies on deterministic TAP naming instead of Cloud Hypervisor runtime TAP discovery, because Cloud Hypervisor does not reliably reflect auto-generated TAP device names back into VM info after boot.

This makes the current networking integration a practical first step: Odorobo can transform user-friendly network references into stable Cloud Hypervisor network config while keeping host-side naming deterministic and debuggable.