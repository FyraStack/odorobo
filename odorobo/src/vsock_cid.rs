//! Persistent, node-local virtio-vsock guest CID allocation.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use stable_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use ulid::Ulid;

const FIRST_GUEST_CID: u32 = 3;
const MAX_GUEST_CID: u32 = u32::MAX - 1;
const REGISTRY_VERSION: u32 = 1;
const REGISTRY_ENV: &str = "ODOROBO_VSOCK_CID_REGISTRY";
const DEFAULT_REGISTRY: &str = "/var/lib/odorobo/vsock-cids.json";
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct VsockCidAllocator {
    registry_path: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct VsockCidReservation {
    pub cid: u32,
    previous_cid: Option<u32>,
    previous_process_exited: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    assignments: BTreeMap<String, u32>,
    #[serde(default)]
    process_exited: BTreeSet<String>,
}

impl VsockCidAllocator {
    pub fn new<P: Into<PathBuf>>(registry_path: P) -> Self {
        Self {
            registry_path: registry_path.into(),
        }
    }

    pub fn from_environment() -> Self {
        let path = std::env::var_os(REGISTRY_ENV)
            .map_or_else(|| PathBuf::from(DEFAULT_REGISTRY), PathBuf::from);
        Self::new(path)
    }

    /// Reserve a CID and retain enough information to undo the change if VM
    /// startup fails. CID 0-2 and `u32::MAX` are reserved by the transport.
    pub fn reserve_with_previous(
        &self,
        vmid: Ulid,
        requested: Option<u32>,
    ) -> Result<VsockCidReservation> {
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            let existing = registry.assignments.get(&vmid).copied();
            let previous_process_exited = registry.process_exited.contains(&vmid);
            if existing.is_some() && !previous_process_exited {
                return Err(eyre!("VM {vmid} already has an active vsock CID lease; previous process exit must be confirmed before reuse"));
            }
            let cid = match requested {
                Some(cid) if cid < FIRST_GUEST_CID => {
                    return Err(eyre!("vsock guest CID {cid} is reserved; use a CID >= 3"));
                }
                Some(cid) if cid > MAX_GUEST_CID => {
                    return Err(eyre!("vsock guest CID {cid} is reserved"));
                }
                Some(cid) => cid,
                None if let Some(cid) = existing => cid,
                None => {
                    let used: HashSet<_> = registry.assignments.values().copied().collect();
                    (FIRST_GUEST_CID..=MAX_GUEST_CID)
                        .find(|cid| !used.contains(cid))
                        .ok_or_else(|| eyre!("no free vsock guest CIDs remain"))?
                }
            };

            if let Some((other_vm, _)) = registry
                .assignments
                .iter()
                .find(|(other_vm, other_cid)| **other_cid == cid && **other_vm != vmid)
            {
                return Err(eyre!(
                    "vsock guest CID {cid} is already assigned to VM {other_vm}"
                ));
            }

            if existing != Some(cid) {
                registry.assignments.insert(vmid.clone(), cid);
            }
            registry.process_exited.remove(&vmid);
            Ok(VsockCidReservation {
                cid,
                previous_cid: existing,
                previous_process_exited,
            })
        })
    }

    /// Restore the prior assignment after a failed startup, but only if the
    /// registry still contains the reservation made by this attempt.
    pub fn rollback(&self, vmid: Ulid, reservation: VsockCidReservation) -> Result<()> {
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            if registry.assignments.get(&vmid).copied() != Some(reservation.cid) {
                return Ok(());
            }
            if let Some(previous_cid) = reservation.previous_cid {
                if registry
                    .assignments
                    .iter()
                    .any(|(other_vm, cid)| *cid == previous_cid && *other_vm != vmid)
                {
                    return Err(eyre!(
                        "cannot restore previous vsock CID {previous_cid}; it was assigned to another VM"
                    ));
                }
                registry.assignments.insert(vmid.clone(), previous_cid);
                if reservation.previous_process_exited {
                    registry.process_exited.insert(vmid);
                }
            } else {
                registry.assignments.remove(&vmid);
                registry.process_exited.remove(&vmid);
            }
            Ok(())
        })
    }

    /// Record that the VMM process exited. Shutdown retains the assignment,
    /// but this proof lets a later explicit delete safely release it.
    pub fn mark_process_exited(&self, vmid: Ulid) -> Result<()> {
        if !self.registry_path.try_exists()? {
            return Ok(());
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            if registry.assignments.contains_key(&vmid) {
                registry.process_exited.insert(vmid);
            }
            Ok(())
        })
    }

    /// Release an assignment after the caller has confirmed process exit.
    pub fn release(&self, vmid: Ulid) -> Result<()> {
        if !self.registry_path.try_exists()? {
            return Ok(());
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            registry.assignments.remove(&vmid);
            registry.process_exited.remove(&vmid);
            Ok(())
        })
    }

    /// Release a persisted lease only when the VM actor previously recorded a
    /// confirmed process exit. Returns whether a lease was present; callers use
    /// `false` to distinguish an untracked VM from a safely released lease.
    pub fn release_stopped(&self, vmid: Ulid) -> Result<bool> {
        if !self.registry_path.try_exists()? {
            return Ok(false);
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            let had_assignment = registry.assignments.contains_key(&vmid);
            if had_assignment && !registry.process_exited.contains(&vmid) {
                return Err(eyre!(
                    "cannot release vsock CID for VM {vmid}: VMM process exit was not confirmed"
                ));
            }
            registry.assignments.remove(&vmid);
            registry.process_exited.remove(&vmid);
            Ok(had_assignment)
        })
    }

    fn with_registry<T>(&self, update: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let parent = self
            .registry_path
            .parent()
            .ok_or_else(|| eyre!("vsock CID registry path has no parent"))?;
        fs::create_dir_all(parent).wrap_err("failed to create vsock CID registry directory")?;

        let lock_path = self.registry_path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(lock_path)
            .wrap_err("failed to open vsock CID registry lock")?;
        lock.lock_exclusive()
            .wrap_err("failed to lock vsock CID registry")?;

        let mut registry = self.read_registry()?;
        let result = update(&mut registry)?;
        self.write_registry(&registry)?;
        FileExt::unlock(&lock).wrap_err("failed to unlock vsock CID registry")?;
        Ok(result)
    }

    fn read_registry(&self) -> Result<Registry> {
        let registry = match fs::read(&self.registry_path) {
            Ok(contents) => serde_json::from_slice::<Registry>(&contents)
                .wrap_err("vsock CID registry is invalid JSON")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry {
                version: REGISTRY_VERSION,
                assignments: BTreeMap::new(),
                process_exited: BTreeSet::new(),
            },
            Err(error) => return Err(error).wrap_err("failed to read vsock CID registry"),
        };
        if registry.version != REGISTRY_VERSION {
            return Err(eyre!(
                "unsupported vsock CID registry version {}",
                registry.version
            ));
        }
        let mut seen = HashSet::new();
        for (vmid, cid) in &registry.assignments {
            if !(FIRST_GUEST_CID..=MAX_GUEST_CID).contains(cid) {
                return Err(eyre!(
                    "vsock CID registry assigns reserved CID {cid} to {vmid}"
                ));
            }
            if !seen.insert(*cid) {
                return Err(eyre!("vsock CID registry contains duplicate CID {cid}"));
            }
        }
        Ok(registry)
    }

    fn write_registry(&self, registry: &Registry) -> Result<()> {
        let parent = self
            .registry_path
            .parent()
            .expect("validated registry parent");
        let suffix = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self
            .registry_path
            .with_extension(format!("json.tmp-{}-{suffix}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temp_path)
                .wrap_err("failed to create temporary vsock CID registry")?;
            serde_json::to_writer_pretty(&mut file, registry)
                .wrap_err("failed to serialize vsock CID registry")?;
            file.write_all(b"\n")
                .wrap_err("failed to finish vsock CID registry")?;
            file.sync_all()
                .wrap_err("failed to sync temporary vsock CID registry")?;
            fs::rename(&temp_path, &self.registry_path)
                .wrap_err("failed to publish vsock CID registry")?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .wrap_err("failed to sync vsock CID registry directory")?;
            Ok(())
        })();
        if result.is_err() {
            drop(fs::remove_file(&temp_path));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator() -> (VsockCidAllocator, PathBuf) {
        let directory =
            std::env::temp_dir().join(format!("odorobo-vsock-cid-{}", Ulid::generate()));
        let path = directory.join("registry.json");
        (VsockCidAllocator::new(path), directory)
    }

    fn reserve(allocator: &VsockCidAllocator, vmid: Ulid, requested: Option<u32>) -> Result<u32> {
        allocator
            .reserve_with_previous(vmid, requested)
            .map(|reservation| reservation.cid)
    }

    #[test]
    fn allocates_unique_cids_and_persists_them_across_allocator_instances() {
        let (allocator, directory) = allocator();
        let first = Ulid::generate();
        let second = Ulid::generate();
        assert_eq!(reserve(&allocator, first, None).unwrap(), FIRST_GUEST_CID);
        assert_eq!(
            reserve(&allocator, second, None).unwrap(),
            FIRST_GUEST_CID + 1
        );

        allocator.mark_process_exited(first).unwrap();
        let reopened = VsockCidAllocator::new(allocator.registry_path);
        assert_eq!(reserve(&reopened, first, None).unwrap(), FIRST_GUEST_CID);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_collisions_and_reserved_cids_but_allows_owner_to_reuse_its_cid() {
        let (allocator, directory) = allocator();
        let first = Ulid::generate();
        let second = Ulid::generate();
        assert_eq!(reserve(&allocator, first, Some(42)).unwrap(), 42);
        drop(reserve(&allocator, first, Some(42)).unwrap_err());
        allocator.mark_process_exited(first).unwrap();
        assert_eq!(reserve(&allocator, first, Some(42)).unwrap(), 42);
        drop(reserve(&allocator, second, Some(42)).unwrap_err());
        drop(reserve(&allocator, second, Some(2)).unwrap_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn release_frees_a_cid_for_reuse() {
        let (allocator, directory) = allocator();
        let first = Ulid::generate();
        let second = Ulid::generate();
        assert_eq!(reserve(&allocator, first, None).unwrap(), FIRST_GUEST_CID);
        allocator.release(first).unwrap();
        assert_eq!(reserve(&allocator, second, None).unwrap(), FIRST_GUEST_CID);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stopped_release_requires_confirmed_exit_and_reservations_reactivate_the_lease() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        assert_eq!(reserve(&allocator, vmid, None).unwrap(), FIRST_GUEST_CID);
        drop(allocator.release_stopped(vmid).unwrap_err());

        allocator.mark_process_exited(vmid).unwrap();
        let retry = allocator.reserve_with_previous(vmid, None).unwrap();
        drop(allocator.release_stopped(vmid).unwrap_err());
        allocator.rollback(vmid, retry).unwrap();
        assert!(allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_removes_new_reservations_and_restores_previous_assignments() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        let initial = allocator.reserve_with_previous(vmid, None).unwrap();
        assert_eq!(initial.cid, FIRST_GUEST_CID);
        allocator.rollback(vmid, initial).unwrap();
        assert_eq!(
            reserve(&allocator, Ulid::generate(), None).unwrap(),
            FIRST_GUEST_CID
        );

        let previous = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        allocator.mark_process_exited(vmid).unwrap();
        let replacement = allocator.reserve_with_previous(vmid, Some(43)).unwrap();
        assert_eq!(replacement.previous_cid, Some(42));
        allocator.rollback(vmid, replacement).unwrap();
        let restored = allocator.reserve_with_previous(vmid, None).unwrap();
        assert_eq!(restored.cid, 42);
        allocator.rollback(vmid, restored).unwrap();
        allocator.rollback(vmid, previous).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cleanup_without_a_registry_does_not_create_runtime_state() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        allocator.mark_process_exited(vmid).unwrap();
        allocator.release(vmid).unwrap();
        assert!(!allocator.release_stopped(vmid).unwrap());
        assert!(!directory.exists());
    }

    #[test]
    fn rejects_wildcard_cid() {
        let (allocator, directory) = allocator();
        drop(
            allocator
                .reserve_with_previous(Ulid::generate(), Some(u32::MAX))
                .unwrap_err(),
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
