# Node networking setup

How to configure hypervisor nodes so the agents find each other and build networks.
`docs/network.md` is the design contract; this page is the operator checklist.

**Status:** every node builds routed networks with their VXLAN devices and configures FRR to
advertise its VMs over BGP EVPN. VMs on different nodes reach each other once the nodes peer in BGP.
The agents do not set up that peering yet: until they do, add the BGP neighbors by hand (see
[FRR](#frr)). Network objects live in the manager's memory only.

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

| Key | Meaning |
|---|---|
| `network.network_mode` | the node-local `default` network: `HostonlyNat` (private bridge, NAT out `upstream_iface`, which defaults to the default-route interface) or `Bridged` (operator-managed bridge with an uplink; `bridge`, `subnet`, `gateway`) |
| `network.dhcp_config` | optional dnsmasq pool on that bridge |
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

Peering is manual for now. One BGP session per pair of nodes is enough: the node with the static
`neighbor` line connects, and the other node accepts it as a dynamic neighbor through its
`listen range`. Configuring both sides also works but is not needed. On each node, add every node
with a lower VTEP as a neighbor:

```bash
sudo vtysh -c 'configure terminal' -c 'router bgp 65000' -c 'neighbor 10.77.0.11 peer-group fabric'
```

Cautions:

- Restarting FRR drops everything the agent configured, because it lives only in the running
  configuration. The agent re-applies its configuration every 30 seconds, so it comes back on its
  own. Manual `neighbor` lines do not; add them again.
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
3. Create networks. The manager sends each network to every agent it knows. When it finds an agent
   it has not seen before (a new node, or a restarted agent), it sends that agent every network in
   its registry. Restarting the manager forgets all networks.

```bash
curl -X POST http://<manager>:3000/networks -H 'content-type: application/json' \
    -d '{"network":{"id":"vmnet1","subnet":"10.40.10.0/24","provider":"routed"}}'
# reply includes the assigned "state":{"vni":<n>}
```

The manager allocates from `1-16777215`, excluding the legacy VNIs `253-255` and PVE SDN's
`10001-11002`. The pool holds 16,776,210 VNIs; node capacity is lower because each network creates
interfaces and an FRR VRF on every agent.

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
| per VM NIC | tap `vmtap-<vm4>-<net4>` in the VRF, gateway `/32` on the tap, `/32` route to the VM in table `16777216 + <n>`, anti-spoof rule in nft chain `nic_guard` |
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

- Automatic BGP peering: agents exchanging VTEPs over the swarm and adding neighbors themselves.
- Swarm pre-shared key, seed peers for routed underlays, and manager persistence (a manager restart
  forgets every network).
- Internet egress for routed tenants.
