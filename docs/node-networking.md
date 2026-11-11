# Node networking setup

How to configure hypervisor nodes so the agents find each other and build networks.
`docs/vm-networking.md` is the design contract; this page is the operator checklist.

**Status:** every node builds routed networks with their VXLAN devices and configures FRR to
advertise its VMs over BGP EVPN. VMs on different nodes reach each other once the nodes peer in BGP.
The agents discover VTEPs and set up that peering themselves (see [FRR](#frr)). Network updates
retry until acknowledged. Network objects and cleanup history live in the manager's memory only.

## Roles

- **Agent:** every hypervisor node runs `odorobo`. It builds networks on the node, attaches VM NICs,
  and runs Cloud Hypervisor.
- **Manager:** exactly one node also runs the manager (`--manager-enabled true`). It serves the API on
  `0.0.0.0:3000`, assigns VNIs, places VMs, and sends network objects to every agent.

## Host requirements

| Requirement | Why |
|---|---|
| `/dev/kvm` | Cloud Hypervisor |
| unique, nonzero `/etc/machine-id` on every Linux node | stable networking identity across agent restarts |
| kernel modules `vrf`, `vxlan`, `bridge` (on Fedora: package `kernel-modules`, not only `kernel-core`) | routed networks |
| `nft` (nftables), `ip` (iproute2), `dnsmasq` | NIC policy, NAT, DHCP for bridged networks |
| `cloud-hypervisor` on `PATH`, `/usr/libexec/virtiofsd`, `skopeo`, `e2fsprogs` | VMs and OCI mode |
| `/var/lib/odorobo/microvm/vmlinux` and `initramfs.cpio.gz` | OCI-mode guest kernel and init (`just initramfs`) |
| firewalld stopped, or forwarding allowed for tenant interfaces | firewalld's `filter_FORWARD` rejects routed tenant traffic |
| FRR (`zebra` and `bgpd` running, `vtysh` on `PATH`), only with `evpn` set | route advertisement between nodes; tested with FRR 10.7 |

The agent changes host-wide settings at start: `net.ipv4.ip_forward=1` and
`net.ipv4.conf.all.rp_filter=0`. It applies per-tap settings (`rp_filter`, `proxy_arp`, ARP policy)
when a NIC attaches.

## Underlay

The underlay is the network VXLAN traffic crosses between nodes.

- Pick one underlay CIDR for the fabric, for example `10.77.0.0/24`. Give every node exactly one
  address inside it, on any interface. Configure the address yourself; the agent never assigns it.
- At start, the agent finds that address and uses it as the node's **VTEP**. The agent refuses to
  start if it finds no address inside the CIDR, or more than one.
- VTEPs must reach each other. Open UDP **4789** (VXLAN) and TCP **179** (BGP) between them.
- Underlay MTU must be at least **1500**. Tenant networks default to MTU 1450, and VXLAN adds 50
  bytes.
- VXLAN on the underlay is unencrypted. Use a trusted network, or put the VTEP on a WireGuard/IPsec
  interface and raise the underlay MTU by that overhead.

Use a separate NIC or VLAN for the underlay if you can. The VTEP can also be the node's management
address.

## Agent configuration

The agent reads `config.json` from its working directory. Command-line flags and `ODOROBO_*`
environment variables override it (for example `--manager-enabled true`).

A node that only hosts routed networks needs the fabric block and nothing else:

```json
{
  "datacenter": "dc1",
  "region": "r1",
  "network": {
    "evpn": { "underlay": "10.77.0.0/24", "asn": 65000 }
  }
}
```

`network_mode` and `dhcp_config` are the legacy node-local `default` network. Add them only for
`net://default` attachments, DHCP guests, or host NAT egress; without them the node has no `default`
network at all.

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
    "evpn": { "underlay": "10.77.0.0/24", "asn": 65000 }
  }
}
```

`NodeVrf` creates a minimal node-only VRF. Guests get static addresses
and reach each other and the host through the VRF; `ip vrf exec vrfdefault …` is how you talk to them
from the host. `upstream_iface` is opt-in and NATs egress through the host, scoped to that interface:

```json
{
  "datacenter": "dc1",
  "region": "r1",
  "network": {
    "network_mode": {
      "NodeVrf": {
        "subnet": "192.168.10.0/24",
        "gateway": "192.168.10.1",
        "upstream_iface": "eth0"
      }
    }
  }
}
```

Nothing about a `NodeVrf` network reaches the fabric: there is no VNI, no VXLAN device and no
advertisement, and it never appears in `GET /networks` — the API creates cluster-wide `routed`
networks only. Its VRF device is `vrfdefault` (the id is always `default`).

| Key | Meaning |
|---|---|
| `network.network_mode` | optional; the node-local `default` network: `HostonlyNat` (private bridge, NAT out `upstream_iface`, which defaults to the default-route interface), `Bridged` (operator-managed bridge with an uplink; `bridge`, `subnet`, `gateway`), or `NodeVrf` (one routed VRF on this node, static guest addresses, no DHCP, nothing announced; `subnet`, optional `gateway`, `mtu`, optional `upstream_iface` for host NAT egress). Without it, the node only realizes networks created through the API |
| `network.dhcp_config` | optional dnsmasq pool on that bridge; needs `network_mode` |
| `network.evpn.underlay` | the fabric's underlay CIDR; the node's one local address inside it becomes its VTEP. Without `evpn`, routed networks stay node-local VRFs with no VXLAN devices |
| `network.evpn.asn` | BGP autonomous system of the fabric, default `65000`. Every node uses the same value |

The whole file can be the same on every node; the VTEP comes from each node's own interfaces.

Run the agent from the directory holding `config.json`, for example as a unit:

```ini
[Service]
WorkingDirectory=/etc/odorobo
ExecStart=/usr/local/bin/odorobo --manager-enabled false
Restart=on-failure
```

Use `--manager-enabled true` on the manager node only.

## FRR

With `evpn` set, the agent configures FRR through `vtysh` and refuses to start if that fails or takes
longer than 30 seconds. It only changes FRR's running configuration and never writes `frr.conf`.

### What to put in place first

In `/etc/frr/daemons`, enable `bgpd` (`zebra` always runs). Keep the packaged `zebra_options`; on
Fedora they include `-s 90000000`, a larger netlink receive buffer.

```ini
bgpd=yes
```

`frr.conf` needs nothing from odorobo. The tested setup has only a hostname and FRR's default
profile (`frr defaults traditional`):

```
hostname node1
```

Start FRR before the agent (`After=frr.service` in the agent's unit).

`frr.conf` may hold your own configuration if it stays clear of what the agent manages:

- No `router bgp` with a different ASN. FRR allows one BGP instance in the default VRF, and the
  agent needs it under `evpn.asn`.
- No `vrf vrf<n>`, `router bgp <asn> vrf vrf<n>`, or `odorobo-vm-routes` entries.
- Reserve the `odorobo-mesh` peer group and its member configuration for the agent.
- No `bgp listen range` covering the underlay on another peer group.
- Underlay routing through another protocol (static routes, OSPF, IS-IS) is fine. Underlay routing
  through BGP has to live in the same `router bgp <asn>`. The agent sets `no bgp default
  ipv4-unicast` there, so activate your IPv4 neighbors explicitly under `address-family ipv4
  unicast`. This combination is untested.
- Nodes running Proxmox SDN EVPN have a generated `frr.conf` with their own `router bgp` and VRFs.
  Running odorobo's fabric next to it is untested and likely conflicts.

At start, the agent adds:

- `router bgp <asn>` with the VTEP as router ID and `no bgp default ipv4-unicast`.
- Peer group `fabric`: iBGP, `update-source <VTEP>`, activated for `l2vpn evpn` with
  `advertise-all-vni`.
- `bgp listen range <underlay> peer-group fabric`, so the node accepts sessions from any underlay
  address.
- Prefix-list and route-map `odorobo-vm-routes`, which pass only `/32` routes.

Per routed network, the agent adds `vrf vrf<n>` with `vni <n>`, and `router bgp <asn> vrf vrf<n>`
with `redistribute kernel route-map odorobo-vm-routes` and `advertise ipv4 unicast`. Deleting the
network removes both and the `vrf` stanza.

The agent discovers network actors and exchanges VTEPs through `GetVtep`. It accepts only peers
with the same ASN and underlay CIDR. The higher VTEP initiates a static `odorobo-mesh` neighbor to
the lower VTEP; the lower accepts a dynamic session through `fabric`. One session per pair is enough.
FRR refuses to turn an existing dynamic neighbor into a static one, so the agent preserves inbound
sessions and manually configured neighbors instead of reassigning them.

The mesh reads running configuration and session state before applying changes. Failed queries do
not authorize deletion. Three failed direct probes plus 120 seconds of grace can expire a peer;
lookup omissions and unfinished probes do not count as failures. Established BGP sessions remain
intact despite control-plane discovery failures. Total discovery failure freezes expiry, so the last
unreachable peer or an unidentified startup peer can remain configured. An operator can remove an
obsolete `odorobo-mesh` member explicitly when its absence is known.

The mesh is a full mesh, so session count grows quadratically. FRR's default dynamic-neighbor limit
is 100; larger fabrics need an operator-set `bgp listen limit` and eventually route reflectors.

Cautions:

- Restarting FRR drops everything the agent configured, because it lives only in the running
  configuration. The agent re-applies its configuration every 30 seconds, so it comes back on its
  own, including automatically managed neighbors. Manual `neighbor` lines do not; add them again.
- Do not run `write memory` on these nodes: it would store per-network state in `frr.conf`, and FRR
  would load stale VRFs at its next start.
- Keep your own FRR configuration out of `router bgp <asn>` and the `odorobo-vm-routes` names. The
  agent fails to start when `frr.conf` already has a `router bgp` with a different ASN, or a
  `bgp listen range` for the underlay on another peer group.
- The agent sets `bgp router-id <VTEP>` and `no bgp default ipv4-unicast` on the default BGP
  instance at every start. A different router ID there resets that instance's sessions, and the
  second setting also applies to neighbors you add yourself.

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
3. Create networks. The manager requires acknowledgement from every known node before reporting
   success. Its five-second maintenance loop retries failed delivery and sends existing intent to
   newly discovered network actors. Restarted actors get fresh acknowledgement state.
4. Check BGP sessions and network acknowledgements before starting workloads. Restarting the manager
   forgets both networks and cleanup history; persistence is not implemented.

```bash
curl -X POST http://<manager>:3000/networks -H 'content-type: application/json' \
    -d '{"network":{"id":"vmnet1","subnet":"10.40.10.0/24","provider":"routed"}}'
# reply includes the assigned "state":{"vni":<n>}
```

The manager allocates from `1-16777215`, excluding the legacy VNIs `253-255` and PVE SDN's
`10001-11002`. The pool holds 16,776,210 VNIs; node capacity is lower because each network creates
interfaces and an FRR VRF on every agent.

## Synchronization recovery

The manager retries unacknowledged updates every five seconds. A successful node reply stops that
revision's retries, even when it arrives after a newer retry attempt has started; a restarted network
actor gets fresh acknowledgement state. Updates carry the manager lifetime and operation revision,
and the node rejects conflicting or stale work. A rejected update changes nothing on the node.

A node follows the last manager lifetime it accepted an update from. Once a new manager (for
example after a manager restart) reaches it, the node refuses every later update from the previous
one, so delayed retries from a dead manager cannot undo the new manager's networks. A second live
manager would see its updates refused with `the manager that sent this update has been replaced`.
The node forgets which managers it has replaced when its agent restarts.

A failed create schedules cleanup of any partial realization. A failed delete restores the retained
network on nodes that already removed it. Cleanup keeps the old network metadata and current VNI
ownership, so replay after a restart cannot delete a replacement network. Recreating an ID waits for
its earlier cleanup acknowledgements.

Disconnected nodes remain cleanup obligations. Their VNIs stay quarantined until those nodes return
and acknowledge removal, using `/etc/machine-id` to recognize the new actor. Keep machine IDs unique
when cloning hosts. This state is not persistent; restarting the manager loses it.

## What a node builds

Reserve routing tables `16777217-33554431` for odorobo on every node. Tenant table IDs are
`16777216 + VNI`, not the VNI itself; host services such as Tailscale keep their own low-numbered
tables. Before upgrading from the old table-equals-VNI layout, stop its VMs and delete the networks
with the old agent. The new agent rejects leftover VRFs with the wrong table mapping.

For a routed network with VNI `<n>`, each node creates:

| Object | Details |
|---|---|
| `vrf<n>` | VRF, routing table `16777216 + <n>` |
| table `16777216 + <n>` | `unreachable default metric 4278198272` (IPv4 and IPv6), so unmatched tenant traffic never falls through to the host's main table |
| `vrfbr<n>` | bridge in the VRF, no addresses, MTU = network MTU, MAC `02:00:<VTEP octets>` (this node's EVPN router MAC) |
| `vrfvx<n>` | VXLAN `id <n> local <VTEP> dstport 4789 nolearning`, only port of `vrfbr<n>`, `learning off`, `neigh_suppress on` |
| per VM NIC | tap `vmtap-<digest>` in the VRF, gateway `/32` on the tap, `/32` route to the VM in table `16777216 + <n>`, anti-spoof rule in nft chain `nic_guard` |
| FRR | `vrf vrf<n>` with `vni <n>`, and `router bgp <asn> vrf vrf<n>` advertising the VRF's `/32`s as EVPN type-5 routes |

For each remote VM, FRR installs a route `via <remote VTEP> dev vrfbr<n> onlink proto bgp`, a
neighbor entry with the remote router MAC on `vrfbr<n>`, and an FDB entry for that MAC on
`vrfvx<n>`.

Deleting the network removes all of these. If devices with these names already exist, the agent
reuses them and reapplies their settings, so restarting an agent is safe.

## Checking a node

```bash
ip -br link | grep -E 'vrf|vmtap'                 # devices per network and NIC
ip -d link show vrfvx<n> | grep -o 'local [0-9.]*'  # the VTEP in use
ip route show vrf vrf<n>                          # per-VM /32s and the unreachable default
sudo nft list chain ip odorobo nic_guard          # anti-spoof rules
journalctl -u odorobo                             # "mDNS discovered peer", "Connected to …"
sudo vtysh -c 'show bgp l2vpn evpn summary'       # BGP sessions; a * marks a dynamic neighbor
sudo vtysh -c 'show vrf vni'                      # VRF to VNI, VXLAN and bridge devices, router MAC
sudo vtysh -c 'show bgp l2vpn evpn route type prefix'  # type-5 routes from every node
```

## Not yet available

- Swarm pre-shared key, seed peers for routed underlays, and manager persistence (a manager restart
  forgets networks, acknowledgements and cleanup history).
- Internet egress for routed tenants.
