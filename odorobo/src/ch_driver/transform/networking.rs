use std::hash::{Hash, Hasher};

use cloud_hypervisor_client::models::{NetConfig, VmConfig};
use siphasher::sip::SipHasher13;
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use tracing::info;

use super::ConfigTransform;

/// Rewrites odorobo-local network URI references into deterministic TAP device names.
///
/// Expected input shape:
/// - `net.id = Some("net://<network_id>")`
///
/// Rewritten output:
/// - `net.id = Some("net://<network_id>")`
/// - `net.tap = Some("vmtap-<digest>")`
///
/// This lets Cloud Hypervisor use a known TAP name instead of relying on runtime
/// auto-generated TAP names, which are not reflected back in VM info reliably,
/// while preserving the odorobo-local URI marker for later hook logic.
///
/// Notes:
/// - The TAP name is a stable digest of `(vmid, network_id)`, so it is unique per
///   NIC and cannot collide by truncating either id.
/// - We intentionally do not create the TAP device here yet; this transformer only
///   mutates the VM config into a host-specific concrete value.
/// - The readable provenance lives in the interface alias the network actor stamps,
///   `odorobo <vmid> <network_id>`.
#[derive(Debug, Clone)]
pub struct NetworkTransform;

const NETWORK_URI_PREFIX: &str = "net://";
const TAP_PREFIX: &str = "vmtap";
/// Interface names are limited to `IFNAMSIZ`, including the terminating NUL byte.
pub const MAX_IFNAME_LEN: usize = 15;
/// Hex digits of the digest that fit after [`TAP_PREFIX`] within [`MAX_IFNAME_LEN`].
const TAP_HASH_DIGITS: usize = MAX_IFNAME_LEN - TAP_PREFIX.len() - 1;

impl ConfigTransform for NetworkTransform {
    #[tracing::instrument(skip(config))]
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        let Some(nets) = config.net.as_mut() else {
            return Ok(());
        };

        for net in nets.iter_mut() {
            rewrite_net_config(vmid, net)?;
        }

        Ok(())
    }
}

fn rewrite_net_config(vmid: &str, net: &mut NetConfig) -> Result<()> {
    let Some(id) = net.id.as_deref() else {
        return Ok(());
    };

    let Some(network_id) = id.strip_prefix(NETWORK_URI_PREFIX) else {
        return Ok(());
    };

    validate_network_id(network_id).wrap_err_with(|| format!("invalid network URI id {id}"))?;

    let tap_name = deterministic_tap_name(vmid, network_id);

    info!(
        vmid = vmid,
        network_id = network_id,
        tap = tap_name,
        "rewriting network URI to deterministic TAP device"
    );

    net.id = Some(id.to_owned());
    net.tap = Some(tap_name);

    Ok(())
}

fn validate_network_id(network_id: &str) -> Result<()> {
    crate::network::validate_network_id(network_id).map_err(|err| eyre!(err.to_string()))
}

/// The `(vmid, network_id)` pair a TAP name is derived from: one TAP per NIC.
#[derive(Hash)]
struct TapIdentity<'a> {
    vmid: &'a str,
    network_id: &'a str,
}

/// Returns the deterministic TAP name for a VM's NIC on a network.
///
/// The name is a digest rather than the readable ids, because both ids can exceed
/// the interface-name length and truncating them collides.
pub fn deterministic_tap_name(vmid: &str, network_id: &str) -> String {
    // Fixed-key SipHash, so the name survives restarts and agent upgrades.
    let mut hasher = SipHasher13::new();
    TapIdentity { vmid, network_id }.hash(&mut hasher);
    let hex = format!("{:x}", hasher.finish());
    let hash: String = hex.chars().take(TAP_HASH_DIGITS).collect();
    format!("{TAP_PREFIX}-{hash}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_hypervisor_client::models::VmConfig;

    #[test]
    fn rewrites_net_uri_into_tap_and_plain_id() {
        let mut config = VmConfig {
            net: Some(vec![NetConfig {
                id: Some("net://devnet".to_owned()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        NetworkTransform
            .transform("01KPB9AMVWXC2D1PF4W6E7MHWT", &mut config)
            .unwrap();

        let net = &config.net.as_ref().unwrap()[0];
        assert_eq!(net.id.as_deref(), Some("net://devnet"));
        assert_eq!(net.tap.as_deref(), Some("vmtap-b5f37edc6"));
    }

    #[test]
    fn tap_names_do_not_collide_when_ids_differ_only_at_the_end() {
        let vmid = "01KPB9AMVWXC2D1PF4W6E7MHWT";
        let other_vmid = "01ARZ3NDEKTSV4RRFFQ69G5MHWT";
        assert_ne!(
            deterministic_tap_name(vmid, "vmnet1"),
            deterministic_tap_name(vmid, "vmnet2")
        );
        assert_ne!(
            deterministic_tap_name(vmid, "vmnet1"),
            deterministic_tap_name(other_vmid, "vmnet1")
        );
    }

    #[test]
    fn tap_names_fill_the_interface_name_limit() {
        let tap = deterministic_tap_name("01KPB9AMVWXC2D1PF4W6E7MHWT", "some-long-network-id");
        assert_eq!(tap.len(), MAX_IFNAME_LEN);
        assert!(tap.starts_with("vmtap-"));
    }

    #[test]
    fn ignores_non_network_uri_ids() {
        let mut config = VmConfig {
            net: Some(vec![NetConfig {
                id: Some("net1".to_owned()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        NetworkTransform.transform("vmid", &mut config).unwrap();

        let net = &config.net.as_ref().unwrap()[0];
        assert_eq!(net.id.as_deref(), Some("net1"));
        assert!(net.tap.is_none());
    }

    #[test]
    fn rejects_invalid_network_id() {
        let mut config = VmConfig {
            net: Some(vec![NetConfig {
                id: Some("net://bad/id".to_owned()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let err = NetworkTransform.transform("vmid", &mut config).unwrap_err();
        assert!(err.to_string().contains("invalid network URI id"));
    }
}
