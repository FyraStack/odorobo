//! The OCI image configuration the agent hands to the guest.
//!
//! The agent writes the image's config blob as `config.json`, adding an
//! `odorobo` object for the launch options it controls, so this is the OCI
//! image configuration document with the process settings under `config`.
//! Unknown fields are ignored on purpose: labels, history and diff IDs are not
//! the init's business, and future OCI additions must not break booting.

use std::io;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct ImageConfigDocument {
    #[serde(default)]
    pub config: ImageConfig,
    #[serde(default, rename = "odorobo")]
    odorobo: OdoroboOptions,
}

/// Launch options odorobo adds next to the image's own config.
#[derive(Debug, Default, Deserialize)]
struct OdoroboOptions {
    #[serde(default)]
    stdin: bool,
    #[serde(default)]
    rootfs: Option<RootfsOptions>,
    #[serde(default)]
    network: Vec<NicOptions>,
    #[serde(default)]
    hostname: Option<String>,
}

/// Static interface configuration the guest init applies itself, one entry per
/// NIC whose addressing the host already knows.
#[derive(Debug, Clone, Deserialize)]
pub struct NicOptions {
    /// MAC address that identifies the interface.
    pub mac: String,
    /// Addresses with prefixes, `10.40.10.7/24`.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Default route gateway.
    #[serde(default)]
    pub gateway: Option<String>,
    /// Interface MTU.
    #[serde(default)]
    pub mtu: Option<u32>,
}

/// Writable-layer contract for the guest root, sent for ephemeral roots.
#[derive(Debug, Default, Deserialize)]
struct RootfsOptions {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    tmpfs_upper_bytes: Option<u64>,
}

/// The OCI `config` object, in the casing the image config uses.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

impl ImageConfigDocument {
    pub fn parse(contents: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// Whether the workload's stdin is attached to the workload console.
    pub const fn stdin(&self) -> bool {
        self.odorobo.stdin
    }

    /// Static interface configuration for the guest.
    pub fn network(&self) -> &[NicOptions] {
        &self.odorobo.network
    }

    /// UTS hostname the workload should see, when the host set one.
    pub fn hostname(&self) -> Option<&str> {
        self.odorobo.hostname.as_deref()
    }

    /// Bytes of tmpfs the guest must provide as the ephemeral root's writable
    /// upper; `None` mounts the image share as the workload root itself.
    pub fn tmpfs_upper_bytes(&self) -> Option<u64> {
        match self.odorobo.rootfs.as_ref() {
            Some(rootfs) if rootfs.mode.as_deref() == Some("ephemeral") => rootfs.tmpfs_upper_bytes,
            _ => None,
        }
    }
}

impl ImageConfig {
    /// `ENTRYPOINT` followed by `CMD`, as a container runtime would exec it.
    pub fn args(&self) -> Vec<String> {
        self.entrypoint
            .iter()
            .flatten()
            .chain(self.cmd.iter().flatten())
            .cloned()
            .collect()
    }

    pub fn env(&self) -> &[String] {
        self.env.as_deref().unwrap_or_default()
    }

    /// Empty `WorkingDir` means the image root, as in the OCI spec.
    pub fn working_dir(&self) -> &str {
        self.working_dir
            .as_deref()
            .filter(|dir| !dir.is_empty())
            .unwrap_or("/")
    }

    /// Empty `User` means root.
    pub fn user_spec(&self) -> Option<&str> {
        self.user
            .as_deref()
            .map(str::trim)
            .filter(|spec| !spec.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_process_fields_from_an_image_config() {
        let document = ImageConfigDocument::parse(
            br#"{
                "architecture": "amd64",
                "os": "linux",
                "config": {
                    "Entrypoint": ["/docker-entrypoint.sh"],
                    "Cmd": ["nginx", "-g", "daemon off;"],
                    "Env": ["PATH=/usr/bin", "NGINX_VERSION=1.27"],
                    "WorkingDir": "/usr/share/nginx",
                    "User": "101:101",
                    "Labels": {"maintainer": "nginx"}
                },
                "rootfs": {"type": "layers", "diff_ids": ["sha256:00"]},
                "history": [{"created_by": "test"}]
            }"#,
        )
        .unwrap();
        let config = document.config;
        assert_eq!(
            config.args(),
            ["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
        assert_eq!(config.env(), ["PATH=/usr/bin", "NGINX_VERSION=1.27"]);
        assert_eq!(config.working_dir(), "/usr/share/nginx");
        assert_eq!(config.user_spec(), Some("101:101"));
    }

    #[test]
    fn applies_defaults_for_absent_fields() {
        let document = ImageConfigDocument::parse(br#"{"config": {"Cmd": null}}"#).unwrap();
        assert_eq!(document.config.args(), Vec::<String>::new());
        assert_eq!(document.config.env(), &[] as &[String]);
        assert_eq!(document.config.working_dir(), "/");
        assert_eq!(document.config.user_spec(), None);
    }

    #[test]
    fn rejects_non_json() {
        let err = ImageConfigDocument::parse(b"not json").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reads_the_ephemeral_rootfs_contract() {
        let document = ImageConfigDocument::parse(
            br#"{"config":{"Cmd":["/bin/x"]},"odorobo":{"rootfs":{"mode":"ephemeral","tmpfs_upper_bytes":67108864}}}"#,
        )
        .unwrap();
        assert_eq!(document.tmpfs_upper_bytes(), Some(64 * 1024 * 1024));
        assert!(!document.stdin());

        let plain = ImageConfigDocument::parse(br#"{"config":{}}"#).unwrap();
        assert_eq!(plain.tmpfs_upper_bytes(), None);
    }

    #[test]
    fn reads_static_network_configuration() {
        let document = ImageConfigDocument::parse(
            br#"{"config":{"Cmd":["/bin/x"]},"odorobo":{"hostname":"01M6YGFHMJFFX6KA8QNEF6GCJ4","network":[{"mac":"46:59:52:67:67:6b","addresses":["10.40.10.7/24"],"gateway":"10.40.10.1","mtu":1450}]}}"#,
        )
        .unwrap();
        let network = document.network();
        assert_eq!(network.len(), 1);
        assert_eq!(network[0].mac, "46:59:52:67:67:6b");
        assert_eq!(network[0].addresses, ["10.40.10.7/24"]);
        assert_eq!(network[0].gateway.as_deref(), Some("10.40.10.1"));
        assert_eq!(network[0].mtu, Some(1450));
        assert_eq!(document.hostname(), Some("01M6YGFHMJFFX6KA8QNEF6GCJ4"));

        let plain = ImageConfigDocument::parse(br#"{"config":{}}"#).unwrap();
        assert!(plain.network().is_empty());
        assert_eq!(plain.hostname(), None);
    }
}
