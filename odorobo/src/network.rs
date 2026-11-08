//! Network objects: per-tenant networks the agent realizes on a node.

use std::net::Ipv4Addr;

use ipnet::Ipv4Net;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default TAP MTU for routed networks.
pub const DEFAULT_MTU: u16 = 1450;

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
    /// Stable identifier; manifests reference it as `net://<id>`.
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
    /// L3 VNI and VRF table id; required for routed networks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vni: Option<u32>,
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

    /// Validates the object; the error message is suitable for API responses.
    pub fn validate(&self) -> Result<(), NetworkError> {
        validate_network_id(&self.id)?;

        let prefix_len = self.subnet.prefix_len();
        if !(1..=30).contains(&prefix_len) {
            return Err(NetworkError::new(format!(
                "network {:?} subnet {} must use a prefix between /1 and /30",
                self.id, self.subnet
            )));
        }

        if let Some(gateway) = self.gateway
            && !self.subnet.contains(&gateway)
        {
            return Err(NetworkError::new(format!(
                "network {:?} gateway {gateway} is outside subnet {}",
                self.id, self.subnet
            )));
        }

        if let Some(mtu) = self.mtu
            && !(576..=65_535).contains(&mtu)
        {
            return Err(NetworkError::new(format!(
                "network {:?} mtu {mtu} must be between 576 and 65535",
                self.id
            )));
        }

        match self.provider {
            NetworkProvider::Routed => {
                let Some(vni) = self.vni else {
                    return Err(NetworkError::new(format!(
                        "routed network {:?} requires a vni",
                        self.id
                    )));
                };
                if vni == 0 || vni > 0x00ff_ffff || matches!(vni, 253..=255) {
                    return Err(NetworkError::new(format!(
                        "network {:?} vni {vni} is not a valid VXLAN network identifier",
                        self.id
                    )));
                }
                if self.bridge.is_some() || self.dhcp.is_some() || self.upstream.is_some() {
                    return Err(NetworkError::new(format!(
                        "routed network {:?} does not take bridge, dhcp or upstream",
                        self.id
                    )));
                }
            }
            NetworkProvider::Bridged => {
                if self.vni.is_some() {
                    return Err(NetworkError::new(format!(
                        "bridged network {:?} does not take a vni",
                        self.id
                    )));
                }
                if self.bridge.as_deref().is_none_or(str::is_empty) {
                    return Err(NetworkError::new(format!(
                        "bridged network {:?} requires a bridge",
                        self.id
                    )));
                }
                if self.upstream.as_deref().is_some_and(str::is_empty) {
                    return Err(NetworkError::new(format!(
                        "network {:?} upstream must not be empty",
                        self.id
                    )));
                }
                if let Some(dhcp) = &self.dhcp {
                    dhcp.validate(&self.id, self.subnet, self.gateway())?;
                }
            }
        }

        Ok(())
    }
}

impl DhcpPool {
    fn validate(
        &self,
        network_id: &str,
        subnet: Ipv4Net,
        gateway: Ipv4Addr,
    ) -> Result<(), NetworkError> {
        if u32::from(self.start) > u32::from(self.end) {
            return Err(NetworkError::new(format!(
                "network {network_id:?} dhcp range {} - {} is inverted",
                self.start, self.end
            )));
        }
        if !subnet.contains(&self.start) || !subnet.contains(&self.end) {
            return Err(NetworkError::new(format!(
                "network {network_id:?} dhcp range {} - {} is outside subnet {subnet}",
                self.start, self.end
            )));
        }
        if (u32::from(self.start)..=u32::from(self.end)).contains(&u32::from(gateway)) {
            return Err(NetworkError::new(format!(
                "network {network_id:?} dhcp range {} - {} contains the gateway {gateway}",
                self.start, self.end
            )));
        }
        if self.lease_time.trim().is_empty() {
            return Err(NetworkError::new(format!(
                "network {network_id:?} dhcp lease time must not be empty"
            )));
        }
        Ok(())
    }
}

/// Validates a network identifier, also used for `net://<id>` manifest URIs.
pub fn validate_network_id(network_id: &str) -> Result<(), NetworkError> {
    if network_id.is_empty() || network_id.len() > 32 {
        return Err(NetworkError::new(format!(
            "network id {network_id:?} must be 1 to 32 characters"
        )));
    }
    if !network_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(NetworkError::new(format!(
            "network id {network_id:?} must contain only letters, digits, '-' and '_'"
        )));
    }
    Ok(())
}

/// Returns the address `offset` hosts past the network address.
fn host_address(subnet: Ipv4Net, offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.network()).saturating_add(offset))
}

/// Invalid network object.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct NetworkError {
    message: String,
}

impl NetworkError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
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
            vni: Some(1001),
            mtu: None,
            bridge: None,
            dhcp: None,
            upstream: None,
        }
    }

    fn bridged() -> Network {
        Network {
            id: "legacy".to_owned(),
            subnet: "10.0.0.0/24".parse().unwrap(),
            gateway: Some("10.0.0.1".parse().unwrap()),
            provider: NetworkProvider::Bridged,
            vni: None,
            mtu: None,
            bridge: Some("vmbr0".to_owned()),
            dhcp: Some(DhcpPool {
                start: "10.0.0.100".parse().unwrap(),
                end: "10.0.0.200".parse().unwrap(),
                lease_time: "12h".to_owned(),
            }),
            upstream: Some("eth0".to_owned()),
        }
    }

    #[test]
    fn routed_network_defaults_the_gateway_to_the_first_host_address() {
        let network = routed();
        network.validate().unwrap();
        assert_eq!(network.gateway(), "10.40.10.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(network.mtu(), DEFAULT_MTU);
    }

    #[test]
    fn routed_network_serializes_with_a_snake_case_provider() {
        let json = serde_json::to_value(routed()).unwrap();
        assert_eq!(json["provider"], "routed");
    }

    #[test]
    fn routed_network_requires_a_valid_vni() {
        let mut network = routed();
        network.vni = None;
        assert!(network.validate().is_err());

        network.vni = Some(0);
        assert!(network.validate().is_err());

        network.vni = Some(0x0100_0000);
        assert!(network.validate().is_err());

        network.vni = Some(253);
        assert!(network.validate().is_err());
    }

    #[test]
    fn routed_network_rejects_bridged_options() {
        let mut network = routed();
        network.bridge = Some("vmbr0".to_owned());
        assert!(network.validate().is_err());

        let mut network = routed();
        network.dhcp = Some(DhcpPool {
            start: "10.40.10.10".parse().unwrap(),
            end: "10.40.10.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(network.validate().is_err());
    }

    #[test]
    fn network_rejects_a_gateway_outside_the_subnet() {
        let mut network = routed();
        network.gateway = Some("10.41.0.1".parse().unwrap());
        assert!(network.validate().is_err());
    }

    #[test]
    fn bridged_network_requires_a_bridge_and_rejects_a_vni() {
        let mut network = bridged();
        network.validate().unwrap();

        network.bridge = None;
        assert!(network.validate().is_err());

        let mut network = bridged();
        network.vni = Some(1001);
        assert!(network.validate().is_err());
    }

    #[test]
    fn bridged_network_validates_the_dhcp_pool() {
        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.0.0.200".parse().unwrap(),
            end: "10.0.0.100".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(network.validate().is_err());

        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.9.0.10".parse().unwrap(),
            end: "10.9.0.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(network.validate().is_err());

        let mut network = bridged();
        network.dhcp = Some(DhcpPool {
            start: "10.0.0.1".parse().unwrap(),
            end: "10.0.0.20".parse().unwrap(),
            lease_time: "12h".to_owned(),
        });
        assert!(network.validate().is_err());
    }

    #[test]
    fn network_rejects_unknown_fields() {
        let json = r#"{"id":"x","subnet":"10.0.0.0/24","provider":"routed","vni":1,"bogus":1}"#;
        serde_json::from_str::<Network>(json).unwrap_err();
    }

    #[test]
    fn network_rejects_invalid_ids() {
        let mut network = routed();
        network.id = "dev/net".to_owned();
        assert!(network.validate().is_err());

        network.id = String::new();
        assert!(network.validate().is_err());

        network.id = "a".repeat(33);
        assert!(network.validate().is_err());
    }
}
