//! Node rootfs policy: bounded sizes read once from the agent config.

use std::sync::OnceLock;

use bytesize::ByteSize;
use stable_eyre::{
    Result,
    eyre::{bail, eyre},
};

use crate::config::Config;

/// A bounded byte size with its default. `key` names the config field for
/// error messages.
pub(super) struct Limit {
    key: &'static str,
    default: ByteSize,
    min: ByteSize,
    max: ByteSize,
}

impl Limit {
    const fn new(key: &'static str, default: ByteSize, min: ByteSize, max: ByteSize) -> Self {
        Self {
            key,
            default,
            min,
            max,
        }
    }

    fn resolve(&self, value: Option<ByteSize>) -> Result<ByteSize> {
        let value = value.unwrap_or(self.default);
        if !(self.min.as_u64()..=self.max.as_u64()).contains(&value.as_u64()) {
            bail!(
                "{}={value} must be between {} and {}",
                self.key,
                self.min,
                self.max
            );
        }
        Ok(value)
    }
}

const MAX_COMPRESSED_BYTES: Limit = Limit::new(
    "rootfs_max_compressed_bytes",
    ByteSize::gib(16),
    ByteSize::mib(1),
    ByteSize::tib(1),
);
const RBD_SIZE: Limit = Limit::new(
    "rootfs_rbd_size",
    ByteSize::gib(1),
    ByteSize::mib(64),
    ByteSize::gib(64),
);
const TMP_SCRATCH: Limit = Limit::new(
    "rootfs_tmp_size",
    ByteSize::mib(64),
    ByteSize::mib(1),
    ByteSize::gib(2),
);
const RUN_SCRATCH: Limit = Limit::new(
    "rootfs_run_size",
    ByteSize::mib(32),
    ByteSize::mib(1),
    ByteSize::gib(2),
);
/// Bounds the guest-side tmpfs behind an ephemeral root's writable `OverlayFS`
/// upper; the host passes the size to the guest init.
const EPHEMERAL_UPPER: Limit = Limit::new(
    "rootfs_ephemeral_size",
    ByteSize::gib(1),
    ByteSize::mib(64),
    ByteSize::gib(64),
);
const VARTMP_SCRATCH: Limit = Limit::new(
    "rootfs_vartmp_size",
    ByteSize::mib(32),
    ByteSize::mib(1),
    ByteSize::gib(2),
);

/// Node policy for OCI rootfs handling, parsed once at startup.
#[derive(Clone, Debug)]
pub struct RootfsLimits {
    pub max_compressed_bytes: ByteSize,
    pub rbd_size: ByteSize,
    pub tmp_scratch: ByteSize,
    pub run_scratch: ByteSize,
    pub vartmp_scratch: ByteSize,
    pub ephemeral_upper: ByteSize,
    pub allow_local_transports: bool,
}

impl RootfsLimits {
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            max_compressed_bytes: MAX_COMPRESSED_BYTES
                .resolve(config.rootfs_max_compressed_bytes)?,
            rbd_size: RBD_SIZE.resolve(config.rootfs_rbd_size)?,
            tmp_scratch: TMP_SCRATCH.resolve(config.rootfs_tmp_size)?,
            run_scratch: RUN_SCRATCH.resolve(config.rootfs_run_size)?,
            vartmp_scratch: VARTMP_SCRATCH.resolve(config.rootfs_vartmp_size)?,
            ephemeral_upper: EPHEMERAL_UPPER.resolve(config.rootfs_ephemeral_size)?,
            // Test builds always allow local transports so fixtures can use
            // `oci:<layout>:tag` refs without touching the process environment.
            allow_local_transports: cfg!(test)
                || config.rootfs_allow_local_transports.unwrap_or(false),
        })
    }
}

impl Default for RootfsLimits {
    fn default() -> Self {
        Self {
            max_compressed_bytes: MAX_COMPRESSED_BYTES.default,
            rbd_size: RBD_SIZE.default,
            tmp_scratch: TMP_SCRATCH.default,
            run_scratch: RUN_SCRATCH.default,
            vartmp_scratch: VARTMP_SCRATCH.default,
            ephemeral_upper: EPHEMERAL_UPPER.default,
            allow_local_transports: cfg!(test),
        }
    }
}

static POLICY: OnceLock<RootfsLimits> = OnceLock::new();

/// Install the process-wide policy parsed from `Config`.
pub fn install(limits: RootfsLimits) -> Result<()> {
    POLICY
        .set(limits)
        .map_err(|_| eyre!("rootfs limits are installed twice"))
}

pub(super) fn policy() -> &'static RootfsLimits {
    POLICY.get_or_init(RootfsLimits::default)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn read_only_scratch_sizes_are_bounded() {
        assert_eq!(TMP_SCRATCH.resolve(None).unwrap(), ByteSize::mib(64));
        assert_eq!(
            TMP_SCRATCH.resolve(Some(ByteSize::gib(2))).unwrap(),
            ByteSize::gib(2)
        );
        assert_eq!(RUN_SCRATCH.resolve(None).unwrap(), ByteSize::mib(32));
        for invalid in [ByteSize::b(0), ByteSize::gib(3)] {
            let _err = TMP_SCRATCH.resolve(Some(invalid)).unwrap_err();
        }
    }

    #[test]
    fn ephemeral_upper_size_is_bounded() {
        assert_eq!(EPHEMERAL_UPPER.resolve(None).unwrap(), ByteSize::gib(1));
        assert_eq!(
            EPHEMERAL_UPPER.resolve(Some(ByteSize::mib(512))).unwrap(),
            ByteSize::mib(512)
        );
        for invalid in [ByteSize::b(0), ByteSize::gib(65)] {
            let _err = EPHEMERAL_UPPER.resolve(Some(invalid)).unwrap_err();
        }
    }

    #[test]
    fn rbd_size_is_bounded() {
        assert_eq!(RBD_SIZE.resolve(None).unwrap(), ByteSize::gib(1));
        assert_eq!(
            RBD_SIZE.resolve(Some(ByteSize::mib(64))).unwrap(),
            ByteSize::mib(64)
        );
        assert_eq!(
            RBD_SIZE.resolve(Some(ByteSize::gib(64))).unwrap(),
            ByteSize::gib(64)
        );
        for invalid in [ByteSize::mib(63), ByteSize::gib(65)] {
            let _err = RBD_SIZE.resolve(Some(invalid)).unwrap_err();
        }
    }

    #[test]
    fn config_sizes_are_bounded_when_the_policy_is_built() {
        let config = Config::parse_from(["odorobo", "--rootfs-tmp-size", "3GiB"]);
        let err = RootfsLimits::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("rootfs_tmp_size"), "{err}");
    }

    #[test]
    fn json_config_sets_rootfs_sizes() {
        let mut config = Config::parse_from(["odorobo"]);
        let json: Config = serde_json::from_str(r#"{"rootfs_tmp_size":"128MiB"}"#).unwrap();
        config.serde_merge_fn(json);
        assert_eq!(
            RootfsLimits::from_config(&config).unwrap().tmp_scratch,
            ByteSize::mib(128)
        );
    }
}
