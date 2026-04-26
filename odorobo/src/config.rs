use ahash::AHashMap;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use std::{net::Ipv4Addr};
use sysinfo::System;
use tracing::{ info, warn};

/// Gets the system hostname
pub fn hostname() -> String {
    System::host_name().unwrap_or("odorobo".into())
}

/// This was requested by katherine. Do not change without asking her.
pub fn default_reserved_vcpus() -> u32 {
    2
}

fn default_datacenter() -> String {
    warn!("No datacenter specified, defaulting to Dev");

    "Dev".into()
}

fn default_region() -> String {
    warn!("No region specified, defaulting to Local");
    "Local".into()
}

fn default_bridge_name() -> String {
    "vmbr0".into()
}

fn default_subnet() -> Ipv4Net {
    "10.0.0.0/24".parse().unwrap()
}

fn default_gateway() -> Ipv4Addr {
    "10.0.0.1".parse().unwrap()
}
/// Infers the default upstream interface from the system's default route
fn default_upstream_iface() -> String {
    // ip route
    let out = std::process::Command::new("ip")
        .arg("route")
        .output()
        .unwrap();
    let output = String::from_utf8(out.stdout).unwrap();

    let default_route = output.lines().find(|l| l.starts_with("default")).unwrap();
    let iface = default_route.split_whitespace().nth(4).unwrap();
    info!("inferring default upstream interface: {}", iface);
    iface.into()
}

/// DHCP server config
///
/// config options for dnsmasq
///
/// this configures what options
// --no-daemon
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DhcpConfig {
    pub range: (Ipv4Addr, Ipv4Addr),
    pub subnet: Ipv4Net,
    /// lease time for DHCP clients
    ///
    /// example: 12h, 6h, 30m
    pub lease_time: String,
}

// TODO: move config into a separate module
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct NetworkConfig {
    pub dhcp_config: Option<DhcpConfig>,
    pub network_mode: NetworkMode,
}

/// L3 routing configuration for guests
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum NetworkMode {
    /// Private guest bridge with host-side gateway and outbound NAT.
    HostonlyNat {
        #[serde(default = "default_bridge_name")]
        bridge: String,
        #[serde(default = "default_subnet")]
        subnet: Ipv4Net,
        #[serde(default = "default_gateway")]
        gateway: Ipv4Addr,
        #[serde(default = "default_upstream_iface")]
        upstream_iface: String,
    },
    /// Flat bridge mode for operator-managed uplinks.
    ///
    /// The agent should only ensure that the bridge exists, is up, and has the
    /// configured host address on it. It should not automatically enslave a
    /// physical uplink into the bridge. Operators are expected to attach the
    /// upstream interface themselves and handle any host networking migration
    /// required for their environment.
    ///
    /// Per-VM TAP devices can still be attached to this bridge in the same way
    /// as NAT mode.
    Bridged {
        bridge: String,
        subnet: Ipv4Net,
        gateway: Ipv4Addr,
    },
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::HostonlyNat {
            bridge: default_bridge_name(),
            subnet: default_subnet(),
            gateway: default_gateway(),
            upstream_iface: default_upstream_iface(),
        }
    }
}

// The infra team wants a config file on the box where they can set info specific for the box its on.
// TODO: Double check with infra team (katherine) if they want any other config on the box.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Config {
    /// The hostname of the agent. Defaults to the system hostname
    /// if not specified in the config file.
    #[serde(default = "hostname")]
    pub hostname: String,
    /// The datacenter the agent is running in.
    #[serde(default = "default_datacenter")]
    pub datacenter: String,
    /// The region the agent is running in.
    #[serde(default = "default_region")]
    pub region: String,
    /// The number of VCPUs reserved for the agent. Defaults to 2.
    #[serde(default = "default_reserved_vcpus")]
    pub reserved_vcpus: u32,
    /// this is just arbitrary data that will be shown but does no config
    /// Arbitrary labels that can be used
    #[serde(default)]
    pub labels: AHashMap<String, String>,
    /// Arbitrary annotations that can be used
    #[serde(default)]
    pub annotations: AHashMap<String, String>,
    /// Is manager enabled
    #[serde(default)]
    pub manager_enabled: bool,
    #[serde(default)]
    pub network: NetworkConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_config_serialize() {
        let config = Config {
            network: NetworkConfig {
                dhcp_config: Some(DhcpConfig {
                    range: (Ipv4Addr::new(10, 10, 1, 100), Ipv4Addr::new(10, 10, 1, 200)),
                    subnet: Ipv4Net::new(Ipv4Addr::new(10, 10, 1, 0), 24).unwrap(),
                    lease_time: "12h".to_string(),
                }),
                network_mode: NetworkMode::HostonlyNat {
                    bridge: "vmbr0".to_string(),
                    gateway: Ipv4Addr::new(10, 10, 100, 1),
                    subnet: Ipv4Net::new(Ipv4Addr::new(10, 10, 100, 0), 24).unwrap(),
                    upstream_iface: default_upstream_iface(),
                },
            },
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        // assert_eq!(json, )
        println!("{}", json);
    }
}
