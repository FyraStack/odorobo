# Network objects: per-tenant VPCs on EVPN L3

**Status:** Implemented on branch `cappy/networking-evpn` for the local routed and bridged paths: network objects with validation, the manager API, the agent registry, per-tap gateways, per-VM routes, VRF isolation, nftables policy, the L3 VNI devices (`vrfbr<vni>`/`vrfvx<vni>`) on nodes with `network.evpn` configured, and FRR EVPN type-5 advertisement of per-VM routes (verified across two nodes). Still to come: automatic BGP peering between agents (peering is configured by hand today) and control-plane IPAM. Until the branch lands, released behavior is the `net://` transform, Host-only NAT / Bridged modes, dnsmasq, and TAPs created by Cloud Hypervisor (`docs/networking.md`).

Operator setup for nodes (underlay, `config.json`, discovery, bring-up order) is in `docs/node-networking.md`.

The per-tap gateway and filtering model is exercised by `dev/netns-gateway-harness.sh` (scratch, gitignored on the dev machine); its assertions are the spec the Rust reconciler has to reproduce.

## Goal

Networks become objects the control plane creates and VMs reference by ID. Each network is an isolated VPC with its own configurable address space (`10.0.0.0/24`, arbitrary, and possibly reused by another network), realized on the nodes that host its VMs with a VRF, an L3 VNI, and EVPN type-5 advertisements from FRR.

The guest-side contract keeps production's shapes, split by what the address is: a public `/32` keeps the on-link default (set up in userspace today, by the guest init once it owns NIC configuration), while a VPC address is handed over with the network's prefix because that is what the kernel command line can express. Routing and enforcement are the same either way: every packet is reached through the host.

## Two documents

**Network (VPC object), desired state per network:**

| Field | Meaning |
|---|---|
| `id` | stable identifier referenced by VM attachments |
| `subnet` | e.g. `10.0.0.0/24`; IPv6 prefix as a second field once the v6 model is fixed |
| `gateway` | optional; defaults to the first host address, `<subnet>.1` |
| `provider` | `routed` (VRF isolation, per-tap gateway, L3 VNI on EVPN nodes) or `bridged` (today's VLAN path) |
| `mtu` | default `1450` (VXLAN tax), must reach the guest NIC |
| `bridge`, `dhcp`, `upstream` | bridged networks only: bridge name, DHCP pool, NAT upstream |

The VNI is **not** desired state. The manager assigns it to routed networks and reports it as status, the way a scheduler reports a binding; operators never supply it. The assignment is a hash of the network `id` with linear probing over `20000-29999`, chosen clear of PVE SDN's `10001-11002`; agents take the assigned number from the network object and rebuild their local realization when it changes. Every reference to a network — VM attachments, logs, API paths, labels — uses its ULID (`id`), never the VNI. Nothing durable may depend on a VNI value: it is cluster-wide runtime state that can be re-created, and if a number is re-assigned, agents rebuild their local realization from scratch.

No derived number may reach stored user state: VM manifests carry the network `id` and the resolved address, the guest config carries no numbers, and persistent rootfs state carries none. That is what keeps a renumber a rebuild instead of a migration.

The manager knows networks through an ephemeral in-process registry (like VM intent today): `POST /networks` creates, `GET /networks` lists, `GET /networks/{id}` reads, `DELETE /networks/{id}` removes. Create ignores any `state` the caller sent, assigns one for routed networks, returns the assigned object, fans it out to every known agent, and records it only once all of them accepted; delete refuses while any agent still has NICs attached. Persistence arrives with the rest of agent/manager state.

**Per-VM attachment:** the existing manifest `networks[]` entry (today's `Network` struct) stays the per-VM NIC description and references the network object by ID. It carries resolved values only: MAC, addresses, delegated prefixes, ports. The agent never allocates addresses. The attachment struct should be renamed so it stops colliding with the new network object.

## Guest contract

- **VPC addresses** (private, inside a network's prefix): the VM gets `<address>/<prefix>` — `10.40.10.7/24` — with `default via <subnet>.1`. The tap is a point-to-point segment, so "on-link" is virtual: the host is the only L2 endpoint a guest's ARP can reach. Peer ARPs are answered by `proxy_arp` scoped to the tap's VRF: only addresses routed in that table get a reply, so a wrong-VRF or unrouted address stays unanswered.
- **Public addresses** (a `/32` out of a public pool, one host per address): the VM gets `<address>/32` with `default via <gateway> onlink`, so every packet — including to addresses in the same range — goes through the gateway. Nothing is ever ARPed but the gateway, so no proxy ARP is involved. This shape needs the init-owned NIC configuration (the kernel's `ip=` parameter cannot express a `/32`); that path now exists, only the control-plane side of public addresses is missing.
- IPv6 mirrors both: `<address>/64` with `default via fe80::1` for VPC addresses, `<address>/128` with an on-link `fe80::1` for public ones.
- The gateway address (`.1`, `fe80::1`) is a per-tap service address on every tap of the network. It is link-local to the guest, never routed to, and never advertised.
- `dev/netns-gateway-harness.sh` covers both shapes: the public `/32` variant (gateway and filtering mechanics without proxy ARP) and the VPC `/24` variant with `proxy_arp` on the taps, IPv4-only — proxy ND needs per-address entries and the guest v6 model is still open.

### Guest NIC configuration (OCI mode, implemented)

The initramfs contains one file — the `init` binary — so "the guest configures its own NIC" means `odorobo-init` carries a small rtnetlink client (deps: `libc`, `serde`, `serde_json`; no busybox, no `ip`). The config document carries a `network` section: per interface, matched by MAC, an address list with prefixes, an optional gateway and MTU, applied before the workload starts. The same object's `hostname` sets the UTS hostname (the VM's ULID); the workload and its children see it.

Consequences: routed OCI VMs take no `ip=` token on the kernel command line — the init is the single source of guest addressing, and `ip=dhcp` remains only for NICs without a static address. The `/32` + on-link public shape and IPv6 are now expressible without the kernel's `ip=` parser; `proxy_arp` on routed taps stays for the `/24` VPC shape.

## Host realization

Per node, per network with members:

- VRF named `vrf<vni>` with table ID = VNI, matching PVE's `vrf-vxlan` convention. Table IDs and device names are node-local: they may follow the VNI, but only the object `id` is a stable reference. L3 VNI devices follow the PVE/FRR idiom: `vrfvx<vni>` (VXLAN device) in `vrfbr<vni>` (bridge). Interface names stay under the 15-character limit and derive from the VNI, so no hashing scheme is needed for them.
- The node joins the fabric through `config.json` `network.evpn`: `underlay` (the fabric's underlay CIDR) and `asn` (default `65000`). Without it, routed networks stay node-local VRFs. At start the agent takes the node's one local IPv4 address inside `underlay` as its VTEP and the VXLAN `local`, and fails to start when there is none or more than one. It never assigns the address; the operator configures it and keeps it routable.
- `vrfbr<vni>` is enslaved to the VRF with no addresses (`addrgenmode none`), the network's MTU, and a router MAC derived from the VTEP (`02:00:<vtep octets>`), so it is unique per node and stable across restarts. `vrfvx<vni>` uses dstport 4789, `nolearning`, and is the bridge's only port, with bridge-port `learning off` and `neigh_suppress on`; remote router MACs and VTEPs come from FRR. If the devices already exist, the agent reuses them and reapplies their controller, MTU, MAC, VTEP and link state.
- Every VRF table ends in `unreachable default metric 4278198272` (IPv4 and IPv6). Without it, a lookup that misses the VRF table continues to the next `ip rule`. The packet then leaves through the host's main-table default route with the tenant source address. The high metric lets any learned default, such as an edge's EVPN `0.0.0.0/0`, win.
- Removing a routed network deletes `vrfvx<vni>`, `vrfbr<vni>`, the VRF, and the unreachable defaults. The defaults sit on `lo`, so they outlive the VRF device unless the agent deletes them.
- Per-VM routes live in the VRF table: `10.0.0.5/32 dev vmtap-…`. These routes are what gets advertised, not the subnet (IPv4 only today; `/128`s and `advertise ipv6 unicast` wait for the IPv6 model), so remote hosts send a VM's traffic to the host holding that VM. Their absence is what would make a subnet aggregate blackhole traffic.
- FRR is driven by vtysh deltas only (`odorobo/src/networking/fabric.rs`). At agent start: `router bgp <asn>`, peer group `fabric` (iBGP, `update-source <vtep>`, l2vpn evpn with `advertise-all-vni`), `bgp listen range <underlay>`, and the `/32`-only route-map `odorobo-vm-routes`. Per network: `vrf <name>` + `vni <n>`, a per-VRF BGP instance with `redistribute kernel route-map odorobo-vm-routes` and `advertise ipv4 unicast`. Removal runs `no vni` first (FRR refuses to drop a VRF's BGP instance while it maps an L3 VNI), then `no router bgp … vrf`, and `no vrf` once the kernel VRF is gone. Zebra imports routes the agent installs as kernel routes whatever their `proto` (only FRR's own protocols are read back as theirs), so `redistribute static` would match nothing. Zebra also imports the unreachable defaults as kernel routes. The redistribution therefore needs a route-map that passes only `/32`s; without it, every node advertises an unreachable `0.0.0.0/0`. `frr.conf` stays a boot-time cache; the agent never regenerates the daemon config.
- Firewalld (Fedora default) rejects forwarded traffic in `filter_FORWARD`; the reconciler has to install an accept path for tenant interfaces as part of its bootstrap. Docker and Tailscale also add forward chains on hosts that have them.
- MTU: shared devices, routed taps and the guest NIC get the network's MTU.

## Enforcement

- Isolation is the VRF. Cross-network traffic has no route, and the unreachable default keeps unmatched traffic from falling through to the main table.
- Per-VM policy is nftables on the tap, evaluated at **prerouting**. At the forward hook, `iifname` is the VRF master for traffic arriving on a slave tap, so per-tap rules placed there silently match nothing; a forward chain can only filter per tenant. This was a real false pass in the first harness version; the harness now asserts both halves (`vrf<vni>` counter moves, tap counter stays zero).
- IPv4 anti-spoof: drop when `iifname <tap>` and `ip saddr != <this VM's address>`.
- IPv6 anti-spoof needs `{ <this VM's address>, fe80::/10, :: }` as the allowed source set, because DAD, neighbour discovery, and MLD use `::` and link-local sources. A bare `!= <address>` breaks the guest's own gateway ping.
- IPv4 source validation: `net.ipv4.conf.all.rp_filter` does not propagate to devices. The effective value is `max(all, dev)`, new interfaces inherit `conf/default` (often `2`, loose), so it has to be set per tap. Strict mode otherwise drops legitimate forwarded traffic in a VRF.
- Proxy ARP (`proxy_arp=1` on routed taps) answers the peer ARPs the guest sends under its guessed mask. The kernel answers only for addresses with a route in the arriving tap's VRF, so cross-network lookups stay unanswered and isolation is unaffected.

## Actor contract

The existing `NetworkAgentActor` stays the single actor and single writer for FRR, nftables, and table/VNI uniqueness on the node. Messages grow into:

- `EnsureNetwork { network }` / `RemoveNetwork { id }` — idempotent upsert and explicit removal of the network-scoped resources (VRF, per-tap gateway addresses, routes, member set, DHCP actor). Removal refuses while members remain.
- `AttachNic { vmid, network_id, tap_name, mac_address, address }` / `DetachNic { vmid, tap_name }` — replaces the tap-only attach; the actor stamps the tap alias, enslaves the tap (VRF for routed, bridge for bridged), and adds the per-tap gateway address, the per-VM route and the anti-spoof rule, updating the member set.
- `LookupNetwork { id }` lets the VM actor resolve attachments before create; `PublishPorts` / `UnpublishPorts` keep working (routed NICs publish through their static address instead of a dnsmasq lease); `Status` reports per-network members.
- `observe()` stays reserved for drift checks (reported, not acted on, until there is a reason).

Rules:

- The VM actor ensures the network before the VM boots and fails the start if it cannot. No half-attached VMs, and the network ID in an attachment must be known. Routed NICs must carry an address; the agent appends `ip=<addr>::<gw>:<subnet mask>::eth<i>:off` to the kernel cmdline for them (the network's mask, not `/32`, which the kernel reads as unset).
- Desired networks and attachments are persisted with the rest of the agent state and replayed at boot, converging by diffing live kernel state. This depends on agent state persistence, which does not exist yet.
- Retire only when desired state is gone and the refcount is zero. A network that realizes but has no members stays.
- `config.json` networks become a `provider: bridged` network object with id `default` at boot, so the VLAN path lives in the same actor instead of forking the code.
- Taps stay Cloud Hypervisor-created, with a name the agent controls: the transform rewrites `net://<id>` to `tap=<name>` (`odorobo/src/ch_driver/transform/networking.rs:51-75`) and `Tap::open_named` runs `TUNSETIFF` with exactly that name, so the device is created as `vmtap-<vmid4>-<net4>` (≤15 chars, unique per VM and network). Names are never left implicit: without `tap=`, Cloud Hypervisor autonames the device (`vmtap%d`, kernel picks the first free index) and the API never reports which name it got. Cloud Hypervisor sets no label, but the kernel has one: the actor stamps the tap's `IFLA_IFALIAS` (`odorobo <vmid> <network-id>`) when it attaches the NIC, so provenance shows up in `ip -d link`. No `fd=` passing in this pass. Without `tap=` or `fd=`, Cloud Hypervisor falls back to `vmtap%d`, the kernel picks the first free index and the name never appears in the VM info — a mode we never use.
- The tap only exists once Cloud Hypervisor boots the VM: `vm.create` stores the config and pre-creates console sockets only (`vmm/src/lib.rs:2492`), and devices and taps are created in `vm.boot` → `Vm::new` → `create_devices` (`vmm/src/device_manager.rs:3228 make_virtio_net_devices`). So the reconciler configures each tap right after boot returns — the guest's virtio-net driver has not activated yet at that point. A strict before-the-guest-can-send ordering, or tap survival across hypervisor restarts, would mean the agent creates a persistent tap itself (`TUNSETIFF` + `TUNSETPERSIST`; `ip link add type tap` is unsupported by the kernel here) and Cloud Hypervisor attaches to it by name — the same configuration step, so it is a later addition, not a redesign.

## Compatibility and migration

- `provider: bridged` keeps today's VLAN 1040 model alive behind the same object and attachment code.
- While tenant addresses stay globally unique, the same tap/gateway/attachment contract works with plain BGP unicast and no VXLAN at all; adding `evpn_l3` later is a control-plane change (announce instead of static route), not an agent or guest change.
- Per-tenant L2 VNIs are deliberately out of scope. They are needed only for genuine L2 semantics, and they move the enforcement point off the host.
- Routed VMs do not live-migrate. Moving one means re-creating the tap on the destination and re-announcing its `/32`; established flows break. (OCI-mode VMs cannot live-migrate anyway.)

## Open questions

1. Whether the network subnet is also advertised as an aggregate (for the edge to blackhole and for reachability when every VM of a network is off-node), or only per-VM routes.
2. IPv6 model: per-VM `/128` plus delegated `/64`s (what production does today) versus one prefix per network.
3. Egress and ingress: how a VPC reaches the internet, and how public `/32`s map to VPC addresses. Published ports do not reach routed VMs today: DNAT happens at prerouting, but the route lookup that follows uses the main table, which has no route into the VRF (it sends the packet out the default route). Either the edge handles ingress for routed tenants, or published addresses get a main-table route via the VRF master.
4. How the edge routers (router-01/02) learn tenant networks, and what changes in `stack/ansible`.
5. Whether realized-but-memberless networks are ever collected automatically, or only by an operator-triggered pass like repository GC.
6. When to add kernel-state observation and drift reporting.