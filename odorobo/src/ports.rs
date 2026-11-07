//! Published `FaaS` ports.
//!
//! Manifests express a published port as `<host>:<guest>` with an optional
//! `/tcp` or `/udp` suffix, matching the container runtimes' shorthand.

use std::{fmt, net::Ipv4Addr, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Transport protocol of a published port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

impl PortProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

impl fmt::Display for PortProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A host port forwarded to a guest port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortMapping {
    pub host: u16,
    pub guest: u16,
    pub protocol: PortProtocol,
}

impl fmt::Display for PortMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.guest)?;
        if self.protocol == PortProtocol::Udp {
            f.write_str("/udp")?;
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PortMappingError {
    #[error("port mapping {0:?} must use <host>:<guest> or <host>:<guest>/<tcp|udp>")]
    InvalidFormat(String),
    #[error("port mapping {0:?} contains an invalid port number")]
    InvalidPort(String),
    #[error("port mapping {0:?} uses unsupported protocol {1:?}")]
    UnsupportedProtocol(String, String),
    #[error("port mapping {0:?} must not use port 0")]
    ZeroPort(String),
}

impl FromStr for PortMapping {
    type Err = PortMappingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (ports, protocol) = match value.split_once('/') {
            Some((ports, protocol)) => (ports, parse_protocol(value, protocol)?),
            None => (value, PortProtocol::Tcp),
        };
        let (host, guest) = ports
            .split_once(':')
            .ok_or_else(|| PortMappingError::InvalidFormat(value.to_owned()))?;
        if guest.contains(':') {
            return Err(PortMappingError::InvalidFormat(value.to_owned()));
        }
        Ok(Self {
            host: parse_port(value, host)?,
            guest: parse_port(value, guest)?,
            protocol,
        })
    }
}

fn parse_protocol(value: &str, protocol: &str) -> Result<PortProtocol, PortMappingError> {
    match protocol {
        "tcp" => Ok(PortProtocol::Tcp),
        "udp" => Ok(PortProtocol::Udp),
        other => Err(PortMappingError::UnsupportedProtocol(
            value.to_owned(),
            other.to_owned(),
        )),
    }
}

fn parse_port(value: &str, port: &str) -> Result<u16, PortMappingError> {
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PortMappingError::InvalidPort(value.to_owned()));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| PortMappingError::InvalidPort(value.to_owned()))?;
    if port == 0 {
        return Err(PortMappingError::ZeroPort(value.to_owned()));
    }
    Ok(port)
}

impl Serialize for PortMapping {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PortMapping {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Finds the address leased to `mac_address` in a dnsmasq lease file.
#[must_use]
pub fn lease_ip(contents: &str, mac_address: &str) -> Option<Ipv4Addr> {
    contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _expiry = fields.next()?;
        let mac = fields.next()?;
        let address = fields.next()?;
        mac.eq_ignore_ascii_case(mac_address)
            .then(|| address.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_tcp_mapping() {
        let mapping: PortMapping = "8080:80".parse().unwrap();
        assert_eq!(
            mapping,
            PortMapping {
                host: 8080,
                guest: 80,
                protocol: PortProtocol::Tcp,
            }
        );
        assert_eq!(mapping.to_string(), "8080:80");
    }

    #[test]
    fn parses_explicit_protocols() {
        assert_eq!(
            "8080:80/udp".parse::<PortMapping>().unwrap().protocol,
            PortProtocol::Udp
        );
        assert_eq!(
            "8080:80/tcp".parse::<PortMapping>().unwrap().protocol,
            PortProtocol::Tcp
        );
        assert_eq!(
            "8080:80/udp".parse::<PortMapping>().unwrap().to_string(),
            "8080:80/udp"
        );
    }

    #[test]
    fn rejects_malformed_mappings() {
        for value in [
            "8080",
            "8080:",
            ":80",
            "0:80",
            "8080:0",
            "a:80",
            "8080:80:90",
        ] {
            assert!(
                value.parse::<PortMapping>().is_err(),
                "{value} should be rejected"
            );
        }
        assert_eq!(
            "8080:80/sctp".parse::<PortMapping>().unwrap_err(),
            PortMappingError::UnsupportedProtocol("8080:80/sctp".into(), "sctp".into())
        );
        assert_eq!(
            "70000:80".parse::<PortMapping>().unwrap_err(),
            PortMappingError::InvalidPort("70000:80".into())
        );
    }

    #[test]
    fn serde_uses_the_string_form() {
        let mapping: PortMapping = serde_json::from_str("\"8080:80/udp\"").unwrap();
        assert_eq!(mapping.to_string(), "8080:80/udp");
        assert_eq!(serde_json::to_string(&mapping).unwrap(), "\"8080:80/udp\"");
    }

    #[test]
    fn reads_lease_for_matching_mac() {
        let leases = "1730000000 aa:bb:cc:dd:ee:ff 10.10.100.42 guest 01:aa:bb:cc:dd:ee:ff\n\
                      1730000000 11:22:33:44:55:66 10.10.100.43 other *\n";
        assert_eq!(
            lease_ip(leases, "AA:BB:CC:DD:EE:FF"),
            Some(Ipv4Addr::new(10, 10, 100, 42))
        );
        assert_eq!(lease_ip(leases, "00:00:00:00:00:00"), None);
    }
}
