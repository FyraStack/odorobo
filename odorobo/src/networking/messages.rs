//! Local messages handled by the network actor.

use std::net::Ipv4Addr;

use kameo::Reply;
use ulid::Ulid;

use crate::network::{Network, NetworkProvider};
use crate::ports::PortMapping;

/// Attaches a NIC TAP device to a network.
///
/// The TAP device is created by Cloud Hypervisor when the VM boots and is
/// configured by the network actor.
#[derive(Debug, Clone)]
pub struct AttachNic {
    /// VM identifier for logging and bookkeeping.
    pub vmid: Ulid,
    /// Network the NIC joins.
    pub network_id: String,
    /// Host TAP device name created for this VM by Cloud Hypervisor.
    pub tap_name: String,
    /// MAC address used for DHCP lease lookups on bridged networks.
    pub mac_address: Option<String>,
    /// Static guest address; required on routed networks.
    pub address: Option<Ipv4Addr>,
}

/// Detaches a NIC TAP device from whichever network holds it.
#[derive(Debug, Clone)]
pub struct DetachNic {
    /// VM identifier for logging and bookkeeping.
    pub vmid: Ulid,
    /// Host TAP device name previously attached for this VM.
    pub tap_name: String,
}

/// Publishes host ports to a booted guest.
///
/// The guest address comes from the attached NIC on a routed network, or from
/// the dnsmasq lease for `mac_address` on a bridged network. Publishing
/// completes once the address is known.
#[derive(Debug, Clone)]
pub struct PublishPorts {
    pub vmid: Ulid,
    pub mac_address: String,
    pub ports: Vec<PortMapping>,
}

/// Removes every published port belonging to a VM.
#[derive(Debug, Clone)]
pub struct UnpublishPorts {
    pub vmid: Ulid,
}

/// Looks up a network object on the local agent.
#[derive(Debug, Clone)]
pub struct LookupNetwork {
    /// Network identifier.
    pub id: String,
}

/// Reply to [`LookupNetwork`].
#[derive(Debug, Clone, Reply)]
pub struct LookupNetworkReply {
    pub network: Option<Network>,
}

/// Requests a snapshot of the local networking actor state.
#[derive(Debug, Clone, Default)]
pub struct Status;

/// Local networking status snapshot returned by the networking actor.
#[derive(Debug, Clone, Default, Reply)]
pub struct NetworkStatus {
    /// Realized networks.
    pub networks: Vec<NetworkSummary>,
}

/// Bookkeeping for one realized network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSummary {
    /// Network identifier.
    pub id: String,
    /// Realization mode.
    pub provider: NetworkProvider,
    /// NICs currently attached to the network.
    pub members: Vec<AttachedNic>,
}

/// Bookkeeping entry for an attached NIC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedNic {
    /// VM identifier associated with the NIC.
    pub vmid: Ulid,
    /// Host TAP device name.
    pub tap_name: String,
    /// Static guest address, if any.
    pub address: Option<Ipv4Addr>,
}
