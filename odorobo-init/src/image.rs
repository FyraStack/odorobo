//! The OCI image configuration the agent hands to the guest.
//!
//! The agent writes the image's config blob verbatim as `config.json`, so
//! this is the OCI image configuration document, with the process settings
//! under `config`. Unknown fields are ignored on purpose: labels, history and
//! diff IDs are not the init's business, and future OCI additions must not
//! break booting.

use std::io;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct ImageConfigDocument {
    #[serde(default)]
    pub config: ImageConfig,
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
}
