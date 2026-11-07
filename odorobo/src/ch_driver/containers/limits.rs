//! Node policy read from `ODOROBO_ROOTFS_*` environment variables: bounded
//! sizes and counts, plus whether manifests may name host-local image sources.

use stable_eyre::{
    Result,
    eyre::{bail, eyre},
};

pub(super) const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// A bounded node limit. Sizes take a `M`, `MB`, `G`, `GB`, `T` or `TB` suffix
/// (case-insensitive, 1024-based); counts are plain integers.
pub(super) struct Limit {
    env: &'static str,
    default: &'static str,
    min: u64,
    max: u64,
    sized: bool,
}

impl Limit {
    const fn size(env: &'static str, default: &'static str, min: u64, max: u64) -> Self {
        Self {
            env,
            default,
            min,
            max,
            sized: true,
        }
    }

    pub(super) fn read_env(&self) -> Result<u64> {
        let value = std::env::var(self.env).unwrap_or_else(|_| self.default.to_owned());
        self.parse(&value)
    }

    pub(super) fn parse(&self, value: &str) -> Result<u64> {
        let env = self.env;
        let value = value.trim();
        let (digits, unit) = value.split_at(
            value
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(value.len()),
        );
        let count = digits
            .parse::<u64>()
            .map_err(|_| eyre!("invalid {env}={value:?}"))?;
        let multiplier = match (self.sized, unit.to_ascii_uppercase().as_str()) {
            (false, "") => 1,
            (true, "M" | "MB") => MIB,
            (true, "G" | "GB") => GIB,
            (true, "T" | "TB") => TIB,
            _ => bail!("unsupported unit {unit:?} in {env}={value:?}"),
        };
        let parsed = count
            .checked_mul(multiplier)
            .ok_or_else(|| eyre!("{env}={value:?} overflows"))?;
        if !(self.min..=self.max).contains(&parsed) {
            bail!(
                "{env}={value:?} must be between {} and {}",
                self.min,
                self.max
            );
        }
        Ok(parsed)
    }
}

pub(super) const MAX_COMPRESSED_BYTES: Limit =
    Limit::size("ODOROBO_ROOTFS_MAX_COMPRESSED_BYTES", "16G", MIB, TIB);
pub(super) const MAX_LAYER_BYTES: Limit =
    Limit::size("ODOROBO_ROOTFS_MAX_LAYER_BYTES", "8G", MIB, TIB);
pub(super) const MAX_IMAGE_BYTES: Limit =
    Limit::size("ODOROBO_ROOTFS_MAX_IMAGE_BYTES", "32G", MIB, TIB);
pub(super) const MAX_IMAGE_ENTRIES: Limit = Limit {
    env: "ODOROBO_ROOTFS_MAX_ENTRIES",
    default: "1000000",
    min: 1,
    max: 10_000_000,
    sized: false,
};
pub(super) const RBD_SIZE: Limit = Limit::size("ODOROBO_ROOTFS_RBD_SIZE", "1G", 64 * MIB, 64 * GIB);
pub(super) const TMP_SCRATCH: Limit = Limit::size("ODOROBO_ROOTFS_TMP_SIZE", "64M", MIB, 2 * GIB);
pub(super) const RUN_SCRATCH: Limit = Limit::size("ODOROBO_ROOTFS_RUN_SIZE", "32M", MIB, 2 * GIB);
/// Bounds the tmpfs behind an ephemeral root's writable `OverlayFS` upper.
pub(super) const EPHEMERAL_STATE: Limit =
    Limit::size("ODOROBO_ROOTFS_EPHEMERAL_SIZE", "1G", 64 * MIB, 64 * GIB);
pub(super) const VARTMP_SCRATCH: Limit =
    Limit::size("ODOROBO_ROOTFS_VARTMP_SIZE", "32M", MIB, 2 * GIB);

/// Whether VM manifests may name host-local skopeo transports (`oci:`,
/// `dir:`, `containers-storage:`, ...). These read arbitrary host paths or
/// images as root, so nodes must opt in with
/// `ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS=1`. Test builds always allow them so
/// fixtures can use `oci:<layout>:tag` refs without racing on process env.
pub(super) fn local_transports_allowed() -> bool {
    cfg!(test) || std::env::var("ODOROBO_ROOTFS_ALLOW_LOCAL_TRANSPORTS").is_ok_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_scratch_size_policy_is_bounded_and_configurable() {
        assert_eq!(TMP_SCRATCH.parse("64M").unwrap(), 64 * MIB);
        assert_eq!(RUN_SCRATCH.parse("2G").unwrap(), 2048 * MIB);
        assert_eq!(RUN_SCRATCH.parse(" 2gb ").unwrap(), 2048 * MIB);
        for invalid in ["0M", "3G", "1K", "1T"] {
            let _err = TMP_SCRATCH.parse(invalid).unwrap_err();
        }
    }

    #[test]
    fn ephemeral_state_size_policy_is_bounded_and_configurable() {
        assert_eq!(EPHEMERAL_STATE.parse("1G").unwrap(), 1024 * MIB);
        assert_eq!(EPHEMERAL_STATE.parse("512M").unwrap(), 512 * MIB);
        for invalid in ["0M", "65G", "1T"] {
            let _err = EPHEMERAL_STATE.parse(invalid).unwrap_err();
        }
    }

    #[test]
    fn rbd_size_policy_is_bounded_and_validated() {
        assert_eq!(RBD_SIZE.parse("64M").unwrap(), 64 * MIB);
        assert_eq!(RBD_SIZE.parse("1G").unwrap(), 1024 * MIB);
        assert_eq!(RBD_SIZE.parse("64G").unwrap(), 64 * 1024 * MIB);
        for invalid in ["63M", "65G", "1Gjunk", "-1G", "99999999999999999T"] {
            let _err = RBD_SIZE.parse(invalid).unwrap_err();
        }
    }

    #[test]
    fn oci_entry_limit_is_a_bounded_plain_count() {
        assert_eq!(MAX_IMAGE_ENTRIES.parse("1000000").unwrap(), 1_000_000);
        for invalid in ["0", "10000001", "1M"] {
            let _err = MAX_IMAGE_ENTRIES.parse(invalid).unwrap_err();
        }
    }
}
