# VM networking

odorobo provides an EVPN-based L3 overlay for VPC networking between VMs across nodes, with VXLAN encapsulation and iBGP peering between agents.

operator setup for nodes (underlay, `config.json`, FRR baseline, bring-up order and checks) is in `docs/node-networking.md`.

## Networks

> [!NOTE]
> todo: save this with etcd or something once we have that

a `Network` object is a tenant-owned VPC with a unique VNI, generated from a hash of the network ID. it has a name, optional description, and a CIDR for the VPC.

the `Network` object and the registry that holds it are in-process memory today, so the actual network config is meant to be persisted in a data plane like etcd or netbox, created for each Stack project/tenant.

when you create a Network, the manager hashes the network ID to a 24-bit VNI, probing forward past the reserved values if that number is already taken, and publishes the object to every known agent as a revisioned `SyncNetwork`. the agents ack it, and the manager retries every five seconds until every one of them has.

once an agent has the network, it creates the VRF and VXLAN devices for it (`vrf<vni>`, `vrfbr<vni>` and `vrfvx<vni>`, with the VRF on routing table `16777216 + vni`), then tells FRR to announce that VNI.

## FRR integration

odorobo talks directly to FRR over vtysh to create iBGP sessions between the agents, and for announcing per-VM routes to other hosts. it only changes the running config and never writes `frr.conf`.

Configuration for FRR should be minimal (only enable BGP and VRF support) and the agent manages the rest of the config ephemerally. other routes and config shouldn't be persisted in FRR, as the agent re-applies its whole config every 30 seconds and reconciles it across the cluster on its own.

currently the agents discover each other with libp2p mDNS, look each other up and call `GetVtep`. if both sides are configured with the same underlay and ASN, the higher VTEP dials the lower one and the lower accepts the connection, which builds a full mesh of iBGP sessions between the agents. VNIs and per-VM routes then travel between them as EVPN.


## CH taps

when a VM boots with a routed network attachment, CH creates a tap device on the host and passes it into the VM. the transform pins the tap name (`vmtap-<digest>` of the vmid and network id) so the agent knows the device before the VM boots. right after boot the agent brings the tap down (CH created it up), enslaves it to the network's VRF, stamps the `odorobo <vmid> <network-id>` alias, puts the per-tap gateway (`.1` as a `/32`) on it, installs the anti-spoof rule and `rp_filter`, and only then brings the tap back up: it never forwards before its guard exists. the VM's `/32` route goes in after that, because the kernel rejects a nexthop route whose device is down. FRR then picks the route up through that VRF's `redistribute kernel` and announces it to the other agents as an EVPN type-5 route.

## Public addresses (planned)

> [!NOTE]
> not implemented. this is the design for giving VMs routable public addresses without putting them on a shared VLAN.

today a VM with a public address sits on a host bridge with the router as its gateway, and the orchestrator installs any extra routes for it. the plan is to make the host the L3 hop and let the agent route and announce the address itself, one VRF per uplink.

the node-local half exists today as the `NodeVrf` network mode (see `docs/node-networking.md`): a routed VRF on one node with static guest addresses, no DHCP and nothing announced. missing are the uplink, the eBGP session and the `/32` announcements.

the address model doesn't change. the VM keeps its public `/32`, an on-link route to the gateway and a default through it. what changes is who answers for the gateway: the host proxy-ARPs the router's address inside the uplink VRF, so guests keep their existing config. the VM's L2 domain then ends at its tap — no bridge, no VLAN stretch, and the uplink port carries the node's MAC instead of one MAC per VM.

the uplink VRF holds the uplink interface, every public tap on the node and the per-VM `/32`s, plus the unreachable default route every VRF gets. a VM that is also in a VPC has a second tap in that VPC's VRF, and its own routing picks the path.

announcements follow the routes: attaching a VM installs `<address>/32 dev vmtap-…`, and one `redistribute kernel` with a route-map limited to the public subnet turns its presence into an eBGP advertisement. detaching deletes the route and the address is withdrawn, so there is no FRR config churn per VM. the session runs per uplink — one per uplink per node, or one per HA router — in its own BGP instance and ASN, never the EVPN instance, and the node's private ASN is stripped on the way to the upstreams.

both sides filter: the node announces only the `/32`s the orchestrator assigned it, and the router accepts only those per neighbor with a `maximum-prefix` backstop. the aggregate blackhole stays, so unallocated addresses still don't loop, and draining a node just shuts its session down and lets the blackhole take over.

IPv6 rides the same session: the `/128` transit plus any delegated `/64` is announced with the node as the next hop, which removes the orchestrator's per-VM static route pushes.

a shared per-uplink table gives up automatic VM-to-VM isolation, since two local `/32`s would route to each other directly and skip the router. one stateless nft rule at the forward hook (`iifname "vmtap*" oifname "vmtap*"` drop) puts the router back in the path, which is what the current model has and what its tenant firewall policy relies on.

uplinks are node-level config (`config.json`, like the EVPN underlay) rather than manager-synced `Network` objects, which stay for VPCs. an existing VLAN can keep running next to the announced `/32`s because both live in the same public subnet.
