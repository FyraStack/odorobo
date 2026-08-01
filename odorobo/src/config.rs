use ahash::AHashMap;
use clap::Parser;
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use std::{net::Ipv4Addr, sync::LazyLock};
use sysinfo::System;
use tracing::{info, warn};

const CONFIG_PATH: &str = "config.json";

fn default_bridge_name() -> String {
    "vmbr0".into()
}

fn default_subnet() -> Ipv4Net {
    "10.0.0.0/24".parse().unwrap()
}

fn default_gateway() -> Ipv4Addr {
    "10.0.0.1".parse().unwrap()
}

fn default_etcd_endpoint() -> String {
    "http://127.0.0.1:2379".to_owned()
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

/// Odorobo configurations.
/// These fields are available as command-line arguments, environment variables, and json fields,
/// with fallbacks set in the above order.
#[fyra_proc_macros::cli(env_prefix = "ODOROBO_", serde_merge_fn)]
#[derive(Parser, Serialize, Deserialize, Default, Debug, Clone)]
pub struct Config {
    /// Whether the manager should be enabled on this instance
    #[clap(long)]
    pub manager_enabled: Option<bool>,
    /// The hostname of the agent. Defaults to the system hostname
    /// if not specified in the config file.
    #[clap(long)]
    pub hostname: Option<String>,
    /// The datacenter the agent is running in.
    #[clap(long)]
    pub datacenter: Option<String>,
    /// The region the agent is running in.
    #[clap(long)]
    pub region: Option<String>,
    /// The number of VCPUs reserved for the agent. Defaults to 2.
    #[clap(long)]
    pub reserved_vcpus: Option<u32>,
    /// this is just arbitrary data that will be shown but does no config
    /// Arbitrary labels that can be used
    #[clap(skip)]
    #[serde(default)]
    pub labels: AHashMap<String, String>,
    /// Arbitrary annotations that can be used
    #[clap(skip)]
    #[serde(default)]
    pub annotations: AHashMap<String, String>,

    #[clap(skip)]
    #[serde(default)]
    pub network: NetworkConfig,

    /// Ignore lockfiles and do not create a lockfile
    #[clap(long, action = clap::ArgAction::SetTrue)]
    pub no_lockfile: Option<bool>,

    /// etcd endpoints used for durable cluster state.
    #[clap(long, value_delimiter = ',')]
    #[serde(default = "default_etcd_endpoints")]
    #[allow(clippy::unnecessary_wraps)]
    pub etcd_endpoints: Option<Vec<String>>,
    /// Optional etcd username.
    #[clap(long)]
    pub etcd_username: Option<String>,
    /// Optional etcd password. Prefer the `ODOROBO_ETCD_PASSWORD` environment variable.
    #[clap(long, env = "ODOROBO_ETCD_PASSWORD", hide_env_values = true)]
    pub etcd_password: Option<String>,
    /// Enable TLS for etcd connections.
    #[clap(long)]
    #[serde(default)]
    pub etcd_tls: Option<bool>,
    /// CA certificate PEM file for etcd TLS.
    #[clap(long)]
    pub etcd_ca_file: Option<String>,
    /// Per-request etcd timeout in milliseconds.
    #[clap(long)]
    #[serde(default = "default_etcd_timeout_ms")]
    #[allow(clippy::unnecessary_wraps)]
    pub etcd_timeout_ms: Option<u64>,
    /// Number of connection attempts when creating the etcd client.
    #[clap(long)]
    #[serde(default = "default_etcd_retries")]
    #[allow(clippy::unnecessary_wraps)]
    pub etcd_retries: Option<u32>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_etcd_endpoints() -> Option<Vec<String>> {
    Some(vec![default_etcd_endpoint()])
}

#[allow(clippy::unnecessary_wraps)]
const fn default_etcd_timeout_ms() -> Option<u64> {
    Some(5_000)
}

#[allow(clippy::unnecessary_wraps)]
const fn default_etcd_retries() -> Option<u32> {
    Some(3)
}

impl Config {
    #[must_use]
    pub fn init() -> Self {
        let mut result = Self::parse();
        if let Ok(fd) =
            std::fs::File::open(CONFIG_PATH).inspect_err(|e| warn!(?e, "cannot open {CONFIG_PATH}"))
        {
            let Ok(json) =
                serde_json::from_reader(fd).inspect_err(|e| warn!("cannot parse json: {e}"))
            else {
                return result;
            };
            result.serde_merge_fn(json);
        }
        result
    }
    #[must_use]
    pub fn get_manager_enabled(&self) -> bool {
        self.manager_enabled.unwrap_or(false)
    }
    #[must_use]
    pub fn get_hostname(&self) -> &str {
        static HOSTNAME: LazyLock<Option<String>> = LazyLock::new(System::host_name);
        (self.hostname.as_deref()).unwrap_or_else(|| HOSTNAME.as_deref().unwrap_or("odorobo"))
    }
    #[must_use]
    pub fn get_datacenter(&self) -> &str {
        static DEFAULT_DATACENTER: LazyLock<&'static str> = LazyLock::new(|| {
            warn!("No datacenter specified, defaulting to Dev");
            "Dev"
        });
        (self.datacenter.as_deref()).unwrap_or_else(|| *DEFAULT_DATACENTER)
    }
    #[must_use]
    pub fn get_region(&self) -> &str {
        static DEFAULT_REGION: LazyLock<&'static str> = LazyLock::new(|| {
            warn!("No region specified, defaulting to Local");
            "Local"
        });
        (self.region.as_deref()).unwrap_or_else(|| *DEFAULT_REGION)
    }
    #[must_use]
    pub fn get_reserved_vcpus(&self) -> u32 {
        self.reserved_vcpus.unwrap_or(2)
    }
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
                    lease_time: "12h".to_owned(),
                }),
                network_mode: NetworkMode::HostonlyNat {
                    bridge: "vmbr0".to_owned(),
                    gateway: Ipv4Addr::new(10, 10, 100, 1),
                    subnet: Ipv4Net::new(Ipv4Addr::new(10, 10, 100, 0), 24).unwrap(),
                    upstream_iface: default_upstream_iface(),
                },
            },
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        // assert_eq!(json, )
        println!("{json}");
    }
}
