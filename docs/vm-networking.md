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

when a VM boots with a routed network attachment, CH creates a tap device on the host and passes it into the VM. the transform pins the tap name (`vmtap-<digest>` of the vmid and network id) so the agent knows the device before the VM boots. right after boot, before the guest can send, the agent enslaves the tap to the network's VRF, stamps the `odorobo <vmid> <network-id>` alias, puts the per-tap gateway (`.1` as a `/32`) on it, and adds the VM's `/32` route plus the anti-spoof rule and `rp_filter`. FRR then picks the route up through that VRF's `redistribute kernel` and announces it to the other agents as an EVPN type-5 route.
