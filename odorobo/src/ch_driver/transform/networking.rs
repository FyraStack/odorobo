use cloud_hypervisor_client::models::{NetConfig, VmConfig};
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
/// - `net.tap = Some("vmtap-<vmid>-<network_id>")`
///
/// This lets Cloud Hypervisor use a known TAP name instead of relying on runtime
/// auto-generated TAP names, which are not reflected back in VM info reliably,
/// while preserving the Odorobo-local URI marker for later hook logic.
///
/// Notes:
/// - The TAP name is deterministic per `(vmid, network_id)`.
/// - We intentionally do not create the TAP device here yet; this transformer only
///   mutates the VM config into a host-specific concrete value.
/// - `network_id` is restricted to a conservative character set suitable for Linux
///   interface naming and CH IDs.
#[derive(Debug, Clone)]
pub struct NetworkTransform;

const NETWORK_URI_PREFIX: &str = "net://";
const TAP_PREFIX: &str = "vmtap";
const MAX_IFNAME_LEN: usize = 15;

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

    net.id = Some(id.to_string());
    net.tap = Some(tap_name);

    Ok(())
}

fn validate_network_id(network_id: &str) -> Result<()> {
    if network_id.is_empty() {
        return Err(eyre!("network id must not be empty"));
    }

    if !network_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(eyre!(
            "network id must contain only ASCII alphanumeric characters, '-' or '_'"
        ));
    }

    Ok(())
}

fn deterministic_tap_name(vmid: &str, network_id: &str) -> String {
    let mut vmid_part = sanitize_component(vmid);
    let mut network_part = sanitize_component(network_id);

    // Linux interface names are typically limited to 15 bytes.
    // Prefer keeping some of both the VMID and network id rather than truncating
    // one side entirely. For VMIDs we keep the suffix rather than the prefix,
    // because ULID prefixes are timestamp-derived and less useful for uniqueness.
    let separator_len = 2; // two '-'
    let fixed_len = TAP_PREFIX.len() + separator_len;
    let available = MAX_IFNAME_LEN.saturating_sub(fixed_len);

    if available == 0 {
        return TAP_PREFIX[..MAX_IFNAME_LEN.min(TAP_PREFIX.len())].to_string();
    }

    let half = available / 2;
    let vmid_budget = half.max(1);
    let network_budget = available.saturating_sub(vmid_budget).max(1);

    if vmid_part.len() > vmid_budget {
        vmid_part = vmid_part[vmid_part.len() - vmid_budget..].to_string();
    }
    if network_part.len() > network_budget {
        network_part = network_part[network_part.len() - network_budget..].to_string();
    }

    let mut tap = format!("{TAP_PREFIX}-{vmid_part}-{network_part}");
    if tap.len() > MAX_IFNAME_LEN {
        tap.truncate(MAX_IFNAME_LEN);
    }
    tap
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_hypervisor_client::models::VmConfig;

    #[test]
    fn rewrites_net_uri_into_tap_and_plain_id() {
        let mut config = VmConfig {
            net: Some(vec![NetConfig {
                id: Some("net://devnet".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        NetworkTransform
            .transform("01KPB9AMVWXC2D1PF4W6E7MHWT", &mut config)
            .unwrap();

        let net = &config.net.as_ref().unwrap()[0];
        assert_eq!(net.id.as_deref(), Some("net://devnet"));
        assert_eq!(net.tap.as_deref(), Some("vmtap-mhwt-vnet"));
    }

    #[test]
    fn ignores_non_network_uri_ids() {
        let mut config = VmConfig {
            net: Some(vec![NetConfig {
                id: Some("net1".to_string()),
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
                id: Some("net://bad/id".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let err = NetworkTransform.transform("vmid", &mut config).unwrap_err();
        assert!(err.to_string().contains("invalid network URI id"));
    }
}
