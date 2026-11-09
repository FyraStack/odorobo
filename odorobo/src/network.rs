//! Network objects: per-tenant networks the agent realizes on a node.

use std::net::Ipv4Addr;
use std::ops::RangeInclusive;

use ipnet::Ipv4Net;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default TAP MTU for routed networks.
pub const DEFAULT_MTU: u16 = 1450;

/// Subnet prefix lengths a network accepts: /1 through /30.
const SUBNET_PREFIX_RANGE: RangeInclusive<u8> = 1..=30;

/// MTU range for network TAPs; 576 is the minimum IPv4 hosts must accept.
const MTU_RANGE: RangeInclusive<u16> = 576..=65_535;

/// Nonzero identifiers in VXLAN's 24-bit field.
pub(crate) const VNI_RANGE: RangeInclusive<u32> = 1..=0x00ff_ffff;

/// Legacy VNI reservations for Linux's default, main and local table numbers.
const RESERVED_VNI_RANGE: RangeInclusive<u32> = 253..=255;

/// How a network is realized on a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProvider {
    /// One VRF per network; the gateway lives on every VM TAP, no bridging.
    Routed,
    /// One host bridge per network with dnsmasq DHCP and optional NAT.
    Bridged,
}

/// DHCP pool served by dnsmasq for a bridged network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DhcpPool {
    /// First address handed out.
    #[schemars(with = "String")]
    pub start: Ipv4Addr,
    /// Last address handed out.
    #[schemars(with = "String")]
    pub end: Ipv4Addr,
    /// dnsmasq lease time, for example `12h`.
    #[serde(default = "default_lease_time")]
    pub lease_time: String,
}

fn default_lease_time() -> String {
    "12h".to_owned()
}

/// Desired state for one tenant network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub id: String,
    /// IPv4 subnet this network allocates from.
    #[schemars(with = "String")]
    pub subnet: Ipv4Net,
    /// Gateway served to guests; defaults to the first host address of `subnet`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub gateway: Option<Ipv4Addr>,
    /// Realization mode.
    pub provider: NetworkProvider,
    /// TAP MTU; routed networks default to [`DEFAULT_MTU`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Host bridge device; required for bridged networks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// dnsmasq pool; bridged networks only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dhcp: Option<DhcpPool>,
    /// Interface to masquerade egress on; bridged networks only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<NetworkState>,
}

/// Runtime state of one network, assigned by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkState {
    pub vni: u32,
}

/// Usable VXLAN network identifiers, excluding the reserved values.
pub(crate) fn vni_valid(vni: u32) -> bool {
    VNI_RANGE.contains(&vni) && !RESERVED_VNI_RANGE.contains(&vni)
}

impl Network {
    /// Returns the gateway address served on TAP devices.
    #[must_use]
    pub fn gateway(&self) -> Ipv4Addr {
        self.gateway.unwrap_or_else(|| host_address(self.subnet, 1))
    }

    /// Returns the TAP MTU for this network.
    #[must_use]
    pub fn mtu(&self) -> u16 {
        self.mtu.unwrap_or(DEFAULT_MTU)
    }

    /// Returns the assigned L3 VNI, if any.
    #[must_use]
    pub fn vni(&self) -> Option<u32> {
        self.state.as_ref().map(|state| state.vni)
    }

    /// Compares the desired state, ignoring the runtime state.
    #[must_use]
    pub fn same_desired(&self, other: &Self) -> bool {
        self.id == other.id
            && self.subnet == other.subnet
            && self.gateway == other.gateway
            && self.provider == other.provider
            && self.mtu == other.mtu
            && self.bridge == other.bridge
            && self.dhcp == other.dhcp
            && self.upstream == other.upstream
    }

    /// Validates the object.
    ///
    /// A routed network may still be unassigned; agents require the assignment.
    pub fn validate(&self) -> Result<(), NetworkError> {
        validate_network_id(&self.id)?;

        if !SUBNET_PREFIX_RANGE.contains(&self.subnet.prefix_len()) {
            return Err(NetworkError::SubnetPrefix {
                id: self.id.clone(),
                subnet: self.subnet,
            });
        }
        if let Some(gateway) = self.gateway
            && !self.subnet.contains(&gateway)
        {
            return Err(NetworkError::GatewayOutsideSubnet {
                id: self.id.clone(),
                gateway,
                subnet: self.subnet,
            });
        }
        if let Some(mtu) = self.mtu
            && !MTU_RANGE.contains(&mtu)
        {
            return Err(NetworkError::MtuOutOfRange {
                id: self.id.clone(),
                mtu,
            });
        }

        match self.provider {
            NetworkProvider::Routed => self.validate_routed(),
            NetworkProvider::Bridged => self.validate_bridged(),
        }
    }

    fn validate_routed(&self) -> Result<(), NetworkError> {
        if let Some(vni) = self.vni()
            && !vni_valid(vni)
        {
            return Err(NetworkError::InvalidVni {
                id: self.id.clone(),
                vni,
            });
        }
        if self.bridge.is_some() {
            return Err(NetworkError::RoutedTakesNoBridge {
                id: self.id.clone(),
            });
        }
        if self.dhcp.is_some() {
            return Err(NetworkError::RoutedTakesNoDhcp {
                id: self.id.clone(),
            });
        }
        if self.upstream.is_some() {
            return Err(NetworkError::RoutedTakesNoUpstream {
                id: self.id.clone(),
            });
        }
        Ok(())
    }

    fn validate_bridged(&self) -> Result<(), NetworkError> {
        if self.state.is_some() {
            return Err(NetworkError::BridgedTakesNoState {
                id: self.id.clone(),
            });
        }
        if self.bridge.as_deref().is_none_or(str::is_empty) {
            return Err(NetworkError::BridgedNeedsBridge {
                id: self.id.clone(),
            });
        }
        if self.upstream.as_deref().is_some_and(str::is_empty) {
            return Err(NetworkError::EmptyUpstream {
                id: self.id.clone(),
            });
        }
        if let Some(dhcp) = &self.dhcp {
            dhcp.validate(&self.id, self.subnet, self.gateway())?;
        }
        Ok(())
    }
}

impl DhcpPool {
    /// True when both pool ends are inside `subnet`.
    fn range_in_subnet(&self, subnet: Ipv4Net) -> bool {
        subnet.contains(&self.start) && subnet.contains(&self.end)
    }

    fn validate(
        &self,
        network_id: &str,
        subnet: Ipv4Net,
        gateway: Ipv4Addr,
    ) -> Result<(), NetworkError> {
        if self.start > self.end {
            return Err(NetworkError::DhcpRangeInverted {
                id: network_id.to_owned(),
                start: self.start,
                end: self.end,
            });
        }
        if !self.range_in_subnet(subnet) {
            return Err(NetworkError::DhcpRangeOutsideSubnet {
                id: network_id.to_owned(),
                start: self.start,
                end: self.end,
                subnet,
            });
        }
        if (self.start..=self.end).contains(&gateway) {
            return Err(NetworkError::DhcpRangeHoldsGateway {
                id: network_id.to_owned(),
                start: self.start,
                end: self.end,
                gateway,
            });
        }
        if self.lease_time.trim().is_empty() {
            return Err(NetworkError::EmptyLeaseTime {
                id: network_id.to_owned(),
            });
        }
        Ok(())
    }
}

fn nid_chars_valid(nid: &str) -> bool {
    nid.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validates a network identifier, also used for `net://<id>` manifest URIs.
pub fn validate_network_id(network_id: &str) -> Result<(), NetworkError> {
    if network_id.is_empty() || network_id.len() > 32 {
        return Err(NetworkError::IdLength {
            id: network_id.to_owned(),
        });
    }
    if !nid_chars_valid(network_id) {
        return Err(NetworkError::IdCharset {
            id: network_id.to_owned(),
        });
    }
    Ok(())
}

/// Returns the address `offset` hosts past the network address.
fn host_address(subnet: Ipv4Net, offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.network()).saturating_add(offset))
}

/// Invalid network object.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("network id {id:?} must be 1 to 32 characters")]
    IdLength { id: String },
    #[error("network id {id:?} must contain only letters, digits, '-' and '_'")]
    IdCharset { id: String },
    #[error("network {id:?}: subnet {subnet} must use a prefix between /1 and /30")]
    SubnetPrefix { id: String, subnet: Ipv4Net },
    #[error("network {id:?}: gateway {gateway} is outside subnet {subnet}")]
    GatewayOutsideSubnet {
        id: String,
        gateway: Ipv4Addr,
        subnet: Ipv4Net,
    },
    #[error("network {id:?}: mtu {mtu} must be between 576 and 65535")]
    MtuOutOfRange { id: String, mtu: u16 },
    #[error("network {id:?}: vni {vni} is not a valid VXLAN network identifier")]
    InvalidVni { id: String, vni: u32 },
    #[error("network {id:?}: routed networks do not take a bridge")]
    RoutedTakesNoBridge { id: String },
    #[error("network {id:?}: routed networks do not take dhcp")]
    RoutedTakesNoDhcp { id: String },
    #[error("network {id:?}: routed networks do not take an upstream")]
    RoutedTakesNoUpstream { id: String },
    #[error("network {id:?}: bridged networks do not take runtime state")]
    BridgedTakesNoState { id: String },
    #[error("network {id:?}: bridged networks require a bridge")]
    BridgedNeedsBridge { id: String },
    #[error("network {id:?}: upstream must not be empty")]
    EmptyUpstream { id: String },
    #[error("network {id:?}: dhcp range {start} - {end} is inverted")]
    DhcpRangeInverted {
        id: String,
        start: Ipv4Addr,
        end: Ipv4Addr,
    },
    #[error("network {id:?}: dhcp range {start} - {end} is outside subnet {subnet}")]
    DhcpRangeOutsideSubnet {
        id: String,
        start: Ipv4Addr,
        end: Ipv4Addr,
        subnet: Ipv4Net,
    },
    #[error("network {id:?}: dhcp range {start} - {end} contains the gateway {gateway}")]
    DhcpRangeHoldsGateway {
        id: String,
        start: Ipv4Addr,
        end: Ipv4Addr,
        gateway: Ipv4Addr,
    },
    #[error("network {id:?}: dhcp lease time must not be empty")]
    EmptyLeaseTime { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed() -> Network {
        Network {
            id: "devnet".to_owned(),
            subnet: "10.40.10.0/24".parse().unwrap(),
            gateway: None,
            provider: NetworkProvider::Routed,
            mtu: None,
            bridge: None,
            dhcp: None,
            upstream: None,
            state: Some(NetworkState { vni: 1001 }),
        }
    }

    fn bridged() -> Network {
        Network {
            id: "legacy".to_owned(),
            subnet: "10.0.0.0/24".parse().unwrap(),
            gateway: Some("10.0.0.1".parse().unwrap()),
            provider: NetworkProvider::Bridged,
            mtu: None,
            bridge: Some("vmbr0".to_owned()),
            dhcp: Some(DhcpPool {
                start: "10.0.0.100".parse().unwrap(),
                end: "10.0.0.200".parse().unwrap(),
                lease_time: "12h".to_owned(),
            }),
            upstream: Some("eth0".to_owned()),
            state: None,
        }
    }

    fn expect_invalid(network: &Network) -> NetworkError {
        network
            .validate()
            .expect_err("network should have failed validation")
    }

    #[test]
    fn routed_network_defaults_the_gateway_to_the_first_host_address() {
        let network = routed();
        network.validate().unwrap();
        assert_eq!(network.gateway(), "10.40.10.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(network.mtu(), DEFAULT_MTU);
    }

    #[test]
    fn routed_network_serializes_to_its_api_shape() {
        let json = serde_json::to_value(routed()).unwrap();
        assert_eq!(json["provider"], "routed");
        assert_eq!(json["state"]["vni"], 1001);
    }

    #[test]
    fn routed_network_accepts_an_unassigned_vni_and_rejects_invalid_ones() {
        let mut network = routed();
        network.state = None;
        network.validate().unwrap();

        network.state = Some(NetworkState { vni: 0 });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::InvalidVni { vni: 0, .. }
        ));

        network.state = Some(NetworkState { vni: 0x0100_0000 });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::InvalidVni { .. }
        ));

        network.state = Some(NetworkState { vni: 253 });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::InvalidVni { vni: 253, .. }
        ));
    }

    #[test]
    fn same_desired_ignores_the_runtime_state() {
        let assigned = routed();
        assigned.validate().unwrap();
        assert!(!assigned.same_desired(&bridged()));

        let mut unassigned = routed();
        unassigned.state = None;
        assert!(assigned.same_desired(&unassigned));

        unassigned.subnet = "10.41.0.0/24".parse().unwrap();
        assert!(!assigned.same_desired(&unassigned));
    }

    #[test]
    fn routed_network_rejects_bridged_options() {
        let mut network = routed();
        network.bridge = Some("vmbr0".to_owned());
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::RoutedTakesNoBridge { .. }
        ));

        let mut network = routed();
        network.dhcp = Some(DhcpPool {
            start: "10.40.10.10".parse().unwrap(),
            end: "10.40.10.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::RoutedTakesNoDhcp { .. }
        ));
    }

    #[test]
    fn network_rejects_a_gateway_outside_the_subnet() {
        let mut network = routed();
        network.gateway = Some("10.41.0.1".parse().unwrap());
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::GatewayOutsideSubnet { .. }
        ));
    }

    #[test]
    fn bridged_network_requires_a_bridge_and_rejects_runtime_state() {
        bridged().validate().unwrap();

        let mut network = bridged();
        network.bridge = None;
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::BridgedNeedsBridge { .. }
        ));

        let mut network = bridged();
        network.state = Some(NetworkState { vni: 1001 });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::BridgedTakesNoState { .. }
        ));
    }

    #[test]
    fn bridged_network_validates_the_dhcp_pool() {
        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.0.0.200".parse().unwrap(),
            end: "10.0.0.100".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::DhcpRangeInverted { .. }
        ));

        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.9.0.10".parse().unwrap(),
            end: "10.9.0.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::DhcpRangeOutsideSubnet { .. }
        ));

        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.0.0.1".parse().unwrap(),
            end: "10.0.0.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(matches!(
            expect_invalid(&network),
            NetworkError::DhcpRangeHoldsGateway { .. }
        ));
    }

    #[test]
    fn validate_network_id_rejects_bad_ids() {
        assert!(matches!(
            validate_network_id("dev/net"),
            Err(NetworkError::IdCharset { .. })
        ));
        assert!(matches!(
            validate_network_id(""),
            Err(NetworkError::IdLength { .. })
        ));
        assert!(matches!(
            validate_network_id(&"a".repeat(33)),
            Err(NetworkError::IdLength { .. })
        ));
    }
}
