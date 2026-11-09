# Node networking setup

How to configure hypervisor nodes so the agents find each other and build networks.
`docs/network.md` is the design contract; this page is the operator checklist.

**Status:** every node builds routed networks with their VXLAN devices, but nodes do not exchange
routes yet. VMs on the same node reach each other. VMs on different nodes cannot reach each other
until FRR/EVPN route advertisement is implemented. Network objects live in the manager's memory only.

## Roles

- **Agent:** every hypervisor node runs `odorobo`. It builds networks on the node, attaches VM NICs,
  and runs Cloud Hypervisor.
- **Manager:** exactly one node also runs the manager (`--manager-enabled true`). It serves the API on
  `0.0.0.0:3000`, assigns VNIs, places VMs, and sends network objects to every agent.

## Host requirements

| Requirement | Why |
|---|---|
| `/dev/kvm` | Cloud Hypervisor |
| kernel modules `vrf`, `vxlan`, `bridge` (on Fedora: package `kernel-modules`, not only `kernel-core`) | routed networks |
| `nft` (nftables), `ip` (iproute2), `dnsmasq` | NIC policy, NAT, DHCP for bridged networks |
| `cloud-hypervisor` on `PATH`, `/usr/libexec/virtiofsd`, `skopeo`, `e2fsprogs` | VMs and OCI mode |
| `/var/lib/odorobo/microvm/vmlinux` and `initramfs.cpio.gz` | OCI-mode guest kernel and init (`just initramfs`) |
| firewalld stopped, or forwarding allowed for tenant interfaces | firewalld's `filter_FORWARD` rejects routed tenant traffic |

The agent changes host-wide settings at start: `net.ipv4.ip_forward=1` and
`net.ipv4.conf.all.rp_filter=0`. It applies per-tap settings (`rp_filter`, `proxy_arp`, ARP policy)
when a NIC attaches.

## Underlay

The underlay is the network VXLAN traffic crosses between nodes.

- Give every node one address on it. That address is the node's **VTEP**. Configure it on an
  interface yourself: the agent uses the address but never assigns it.
- VTEPs must reach each other, and UDP **4789** must be open between them.
- Underlay MTU must be at least **1500**. Tenant networks default to MTU 1450, and VXLAN adds 50
  bytes.
- VXLAN on the underlay is unencrypted. Use a trusted network, or put the VTEP on a WireGuard/IPsec
  interface and raise the underlay MTU by that overhead.

Use a separate NIC or VLAN for the underlay if you can. The VTEP can also be the node's management
address.

## Agent configuration

The agent reads `config.json` from its working directory. Command-line flags and `ODOROBO_*`
environment variables override it (for example `--manager-enabled true`).

```json
{
  "datacenter": "dc1",
  "region": "r1",
  "network": {
    "network_mode": {
      "HostonlyNat": {
        "bridge": "vmbr0",
        "subnet": "10.10.100.0/24",
        "gateway": "10.10.100.1",
        "upstream_iface": "eth0"
      }
    },
    "dhcp_config": {
      "range": ["10.10.100.100", "10.10.100.200"],
      "subnet": "10.10.100.0/24",
      "lease_time": "12h"
    },
    "evpn": { "vtep": "10.77.0.11" }
  }
}
```

| Key | Meaning |
|---|---|
| `network.network_mode` | the node-local `default` network: `HostonlyNat` (private bridge, NAT out `upstream_iface`, which defaults to the default-route interface) or `Bridged` (operator-managed bridge with an uplink; `bridge`, `subnet`, `gateway`) |
| `network.dhcp_config` | optional dnsmasq pool on that bridge |
| `network.evpn.vtep` | this node's underlay address. Without `evpn`, routed networks stay node-local VRFs with no VXLAN devices |

All other keys can be the same on every node. `vtep` must be unique per node, it being the node's underlay address.

Run the agent from the directory holding `config.json`, for example as a unit:

```ini
[Service]
WorkingDirectory=/etc/odorobo
ExecStart=/usr/local/bin/odorobo --manager-enabled false
Restart=on-failure
```

Use `--manager-enabled true` on the manager node only.

## Discovery

Agents and the manager find each other with libp2p mDNS and connect over TCP on a random port.

- All nodes must share one L2 segment for mDNS. There is no seed list yet.
- Allow UDP 5353 (mDNS) and inbound TCP between nodes. libp2p picks its TCP port at start.
- The swarm has no pre-shared key yet: any odorobo process on the segment joins it. Keep the segment
  trusted.

## Bring-up order

1. Configure the underlay on every node and check that the VTEPs ping each other.
2. Start the agent on every node, in any order. Wait until the manager node's log shows
   `Connected to <peer id>` once per other node.
3. Create networks. The manager sends a network to the agents once, when you create it, so a node
   whose agent starts later never receives it. Restarting the manager forgets all networks.

```bash
curl -X POST http://<manager>:3000/networks -H 'content-type: application/json' \
    -d '{"network":{"id":"vmnet1","subnet":"10.40.10.0/24","provider":"routed"}}'
# reply includes the assigned "state":{"vni":<n>} (pool 20000-29999)
```

## What a node builds

For a routed network with VNI `<n>`, each node creates:

| Object | Details |
|---|---|
| `vrf<n>` | VRF, routing table `<n>` |
| table `<n>` | `unreachable default metric 4278198272` (IPv4 and IPv6), so unmatched tenant traffic never falls through to the host's main table |
| `vrfbr<n>` | bridge in the VRF, no addresses, MTU = network MTU, MAC `02:00:<VTEP octets>` (this node's EVPN router MAC) |
| `vrfvx<n>` | VXLAN `id <n> local <VTEP> dstport 4789 nolearning`, only port of `vrfbr<n>`, `learning off`, `neigh_suppress on` |
| per VM NIC | tap `vmtap-<vm4>-<net4>` in the VRF, gateway `/32` on the tap, `/32` route to the VM in table `<n>`, anti-spoof rule in nft chain `nic_guard` |

Deleting the network removes all of these. If devices with these names already exist, the agent
reuses them and reapplies their settings, so restarting an agent is safe.

## Checking a node

```bash
ip -br link | grep -E 'vrf|vmtap'                 # devices per network and NIC
ip -d link show vrfvx<n> | grep -o 'local [0-9.]*'  # the VTEP in use
ip route show vrf vrf<n>                          # per-VM /32s and the unreachable default
sudo nft list chain ip odorobo nic_guard          # anti-spoof rules
journalctl -u odorobo                             # "mDNS discovered peer", "Connected to …"
```

## Not yet available

- Cross-node routes (FRR BGP EVPN type-5 via vtysh, or the agent mesh).
- VTEP derived from an underlay CIDR instead of a per-node address.
- Swarm pre-shared key, seed peers for routed underlays, network persistence and re-sync.
- Internet egress for routed tenants.
