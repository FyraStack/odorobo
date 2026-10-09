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
    #[cfg(test)]
    sync_fault: Option<RegistrySyncFault>,
}

/// A registry update was published, but its crash durability is unconfirmed.
///
/// The atomic rename succeeded, but syncing its directory failed. Callers must
/// not treat this as an unchanged registry. For a failed reservation made
/// before spawning a VMM, the caller can safely retain confirmed-exit cleanup
/// state; ordinary validation or pre-publication errors do not provide that
/// guarantee for an existing lease.
#[derive(Debug, thiserror::Error)]
#[error("failed to sync vsock CID registry directory after publishing update: {source}")]
pub struct PublishedRegistryUpdate {
    #[source]
    pub source: std::io::Error,
    /// Present for a reservation even when the directory sync failed after
    /// publication. Only this token may prove that reservation's process exited.
    pub reservation_token: Option<VsockLeaseToken>,
}

#[derive(Debug, thiserror::Error)]
#[error("VM {0} has an active vsock CID lease; previous process exit must be confirmed")]
pub struct ActiveVsockLease(pub Ulid);

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegistrySyncFault {
    TemporaryFile,
    Directory,
}

/// Opaque identity of one reservation, independent of VM ID and CID reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct VsockLeaseToken(Ulid);

#[derive(Clone, Copy, Debug)]
pub struct VsockCidReservation {
    pub cid: u32,
    pub token: VsockLeaseToken,
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
    /// Old registries have no tokens. They can only be released with persisted
    /// exit proof; every new reservation acquires a fresh guarded identity.
    #[serde(default)]
    lease_tokens: BTreeMap<String, VsockLeaseToken>,
}

impl Registry {
    fn owns_lease(&self, vmid: &str, token: VsockLeaseToken) -> Result<bool> {
        if !self.assignments.contains_key(vmid) {
            return Ok(false);
        }
        if self.lease_tokens.get(vmid) != Some(&token) {
            return Err(eyre!("vsock CID lease ownership changed for VM {vmid}"));
        }
        Ok(true)
    }

    fn remove_lease(&mut self, vmid: &str) {
        self.assignments.remove(vmid);
        self.process_exited.remove(vmid);
        self.lease_tokens.remove(vmid);
    }
}

impl VsockCidAllocator {
    pub fn new<P: Into<PathBuf>>(registry_path: P) -> Self {
        Self {
            registry_path: registry_path.into(),
            #[cfg(test)]
            sync_fault: None,
        }
    }

    pub fn from_environment() -> Self {
        let path = std::env::var_os(REGISTRY_ENV)
            .map_or_else(|| PathBuf::from(DEFAULT_REGISTRY), PathBuf::from);
        Self::new(path)
    }

    /// Reserve a CID and retain enough information to undo the change if VM
    /// startup fails. CID 0-2 and `u32::MAX` are reserved by the transport.
    /// An error containing [`PublishedRegistryUpdate`] means this attempt's
    /// active reservation was published; its owned token is included in the error.
    pub fn reserve_with_previous(
        &self,
        vmid: Ulid,
        requested: Option<u32>,
    ) -> Result<VsockCidReservation> {
        let token = VsockLeaseToken(Ulid::generate());
        self.with_registry_token(Some(token), |registry| {
            let lease_owner = vmid;
            let vmid = vmid.to_string();
            let existing = registry.assignments.get(&vmid).copied();
            let previous_process_exited = registry.process_exited.contains(&vmid);
            if existing.is_some() && !previous_process_exited {
                return Err(stable_eyre::Report::new(ActiveVsockLease(lease_owner)));
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
            registry.lease_tokens.insert(vmid, token);
            Ok(VsockCidReservation {
                cid,
                token,
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
            if !registry.owns_lease(&vmid, reservation.token)? {
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
                // Keep this reservation's token: restoring the stopped CID
                // must not revive the authority of any earlier incarnation.
                if reservation.previous_process_exited {
                    registry.process_exited.insert(vmid);
                } else {
                    registry.process_exited.remove(&vmid);
                }
            } else {
                registry.remove_lease(&vmid);
            }
            Ok(())
        })
    }

    /// A new no-vsock incarnation still must not bypass an older active lease.
    /// This read-only check never establishes ownership or exit proof.
    pub fn ensure_no_active_lease(&self, vmid: Ulid) -> Result<()> {
        let registry = self.read_registry()?;
        let key = vmid.to_string();
        if registry.assignments.contains_key(&key) && !registry.process_exited.contains(&key) {
            return Err(stable_eyre::Report::new(ActiveVsockLease(vmid)));
        }
        Ok(())
    }

    /// Record exit only for the exact reservation owned by this VMM. A stale
    /// token cannot establish exit proof for a newer incarnation of the same VM.
    pub fn mark_process_exited_owned(&self, vmid: Ulid, token: VsockLeaseToken) -> Result<()> {
        if !self.registry_path.try_exists()? {
            return Ok(());
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            if registry.owns_lease(&vmid, token)? {
                registry.process_exited.insert(vmid);
            }
            Ok(())
        })
    }

    /// Release after confirmed exit, guarded under the same registry lock as
    /// removal. Never fall back to stopped release after an ownership mismatch.
    pub fn release_owned(&self, vmid: Ulid, token: VsockLeaseToken) -> Result<()> {
        if !self.registry_path.try_exists()? {
            return Ok(());
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            if registry.owns_lease(&vmid, token)? {
                registry.remove_lease(&vmid);
            }
            Ok(())
        })
    }

    /// Unguarded helpers exist only for legacy allocator tests. Production
    /// callers must carry a token or use persisted stopped proof.
    #[cfg(test)]
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

    #[cfg(test)]
    pub fn release(&self, vmid: Ulid) -> Result<()> {
        if !self.registry_path.try_exists()? {
            return Ok(());
        }
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            registry.remove_lease(&vmid);
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
            registry.remove_lease(&vmid);
            Ok(had_assignment)
        })
    }

    fn with_registry<T>(&self, update: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        self.with_registry_token(None, update)
    }

    fn with_registry_token<T>(
        &self,
        reservation_token: Option<VsockLeaseToken>,
        update: impl FnOnce(&mut Registry) -> Result<T>,
    ) -> Result<T> {
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
        self.write_registry(&registry, reservation_token)?;
        // Closing the file releases the lock without a fallible explicit unlock
        // turning a successfully published update into an unclassified error.
        drop(lock);
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
                lease_tokens: BTreeMap::new(),
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

    fn write_registry(
        &self,
        registry: &Registry,
        reservation_token: Option<VsockLeaseToken>,
    ) -> Result<()> {
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
            self.sync_temporary_registry(&file)
                .wrap_err("failed to sync temporary vsock CID registry")?;
            fs::rename(&temp_path, &self.registry_path)
                .wrap_err("failed to publish vsock CID registry")?;
            self.sync_registry_directory(parent)
                .map_err(|source| PublishedRegistryUpdate {
                    source,
                    reservation_token,
                })?;
            Ok(())
        })();
        if result.is_err() {
            drop(fs::remove_file(&temp_path));
        }
        result
    }

    // The allocator carries per-instance fault injection only in test builds.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn sync_temporary_registry(&self, file: &File) -> std::io::Result<()> {
        #[cfg(test)]
        if self.sync_fault == Some(RegistrySyncFault::TemporaryFile) {
            return Err(std::io::Error::other(
                "injected temporary registry sync failure",
            ));
        }
        file.sync_all()
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn sync_registry_directory(&self, parent: &std::path::Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self.sync_fault == Some(RegistrySyncFault::Directory) {
            return Err(std::io::Error::other(
                "injected registry directory sync failure",
            ));
        }
        File::open(parent).and_then(|directory| directory.sync_all())
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
        // Restoring an earlier CID never revives that earlier token's authority.
        drop(allocator.rollback(vmid, previous).unwrap_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_sync_failure_reports_a_published_active_reservation() {
        let (mut allocator, directory) = allocator();
        let vmid = Ulid::generate();
        allocator.sync_fault = Some(RegistrySyncFault::Directory);

        let error = allocator
            .reserve_with_previous(vmid, Some(42))
            .unwrap_err()
            .wrap_err("reservation failed");
        let published = error.downcast_ref::<PublishedRegistryUpdate>().unwrap();
        assert_eq!(published.source.kind(), std::io::ErrorKind::Other);
        assert!(published.to_string().contains("after publishing update"));
        assert!(
            published
                .to_string()
                .contains("injected registry directory sync failure")
        );
        let registry = allocator.read_registry().unwrap();
        assert_eq!(registry.assignments.get(&vmid.to_string()), Some(&42));
        assert!(!registry.process_exited.contains(&vmid.to_string()));
        let token = published
            .reservation_token
            .expect("published reservation carries ownership");
        assert_eq!(registry.lease_tokens.get(&vmid.to_string()), Some(&token));

        // The failed write released its lock. A fresh allocator observes an
        // active lease and can clean it up only with confirmed no-spawn exit.
        let reopened = VsockCidAllocator::new(allocator.registry_path);
        let error = reopened.release_stopped(vmid).unwrap_err();
        assert!(error.downcast_ref::<PublishedRegistryUpdate>().is_none());
        reopened.mark_process_exited_owned(vmid, token).unwrap();
        reopened.release_owned(vmid, token).unwrap();
        assert_eq!(reserve(&reopened, vmid, Some(42)).unwrap(), 42);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_sync_failure_publishes_replacement_and_clears_prior_exit() {
        let (mut allocator, directory) = allocator();
        let vmid = Ulid::generate();
        reserve(&allocator, vmid, Some(42)).unwrap();
        allocator.mark_process_exited(vmid).unwrap();
        allocator.sync_fault = Some(RegistrySyncFault::Directory);

        let error = allocator.reserve_with_previous(vmid, Some(43)).unwrap_err();
        let token = error
            .downcast_ref::<PublishedRegistryUpdate>()
            .unwrap()
            .reservation_token
            .expect("published replacement carries ownership");
        let registry = allocator.read_registry().unwrap();
        assert_eq!(registry.assignments.get(&vmid.to_string()), Some(&43));
        assert!(!registry.process_exited.contains(&vmid.to_string()));
        assert_eq!(registry.lease_tokens.get(&vmid.to_string()), Some(&token));
        allocator.sync_fault = None;
        allocator.mark_process_exited_owned(vmid, token).unwrap();
        allocator.release_owned(vmid, token).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn temporary_sync_failure_does_not_publish_or_change_prior_exit() {
        let (mut allocator, directory) = allocator();
        let vmid = Ulid::generate();
        reserve(&allocator, vmid, Some(42)).unwrap();
        allocator.mark_process_exited(vmid).unwrap();
        let previous_contents = fs::read(&allocator.registry_path).unwrap();
        allocator.sync_fault = Some(RegistrySyncFault::TemporaryFile);

        let error = allocator.reserve_with_previous(vmid, Some(43)).unwrap_err();
        assert!(error.downcast_ref::<PublishedRegistryUpdate>().is_none());
        assert_eq!(
            fs::read(&allocator.registry_path).unwrap(),
            previous_contents
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        allocator.sync_fault = None;
        assert!(allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn validation_errors_do_not_claim_publication_or_change_a_live_lease() {
        let (mut allocator, directory) = allocator();
        let active = Ulid::generate();
        let stopped = Ulid::generate();
        reserve(&allocator, active, Some(42)).unwrap();
        reserve(&allocator, stopped, Some(43)).unwrap();
        allocator.mark_process_exited(stopped).unwrap();
        let previous_contents = fs::read(&allocator.registry_path).unwrap();
        allocator.sync_fault = Some(RegistrySyncFault::Directory);

        for (vmid, requested) in [
            (active, None),
            (stopped, Some(42)),
            (stopped, Some(2)),
            (stopped, Some(u32::MAX)),
        ] {
            let error = allocator
                .reserve_with_previous(vmid, requested)
                .unwrap_err();
            assert!(error.downcast_ref::<PublishedRegistryUpdate>().is_none());
            assert_eq!(
                fs::read(&allocator.registry_path).unwrap(),
                previous_contents
            );
        }
        allocator.sync_fault = None;
        drop(allocator.release_stopped(active).unwrap_err());
        assert!(allocator.release_stopped(stopped).unwrap());
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
    fn stale_tokens_cannot_mark_release_or_rollback_newer_same_cid_reservations() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        let first = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        allocator
            .mark_process_exited_owned(vmid, first.token)
            .unwrap();
        let second = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        assert_ne!(first.token, second.token);
        let before = fs::read(&allocator.registry_path).unwrap();
        drop(
            allocator
                .mark_process_exited_owned(vmid, first.token)
                .unwrap_err(),
        );
        drop(allocator.release_owned(vmid, first.token).unwrap_err());
        drop(allocator.rollback(vmid, first).unwrap_err());
        assert_eq!(fs::read(&allocator.registry_path).unwrap(), before);
        drop(allocator.release_stopped(vmid).unwrap_err());

        // A new persisted exit does not transfer delete authority to an older
        // token, even if a retry previously managed to mark its own exit.
        allocator
            .mark_process_exited_owned(vmid, second.token)
            .unwrap();
        drop(allocator.release_owned(vmid, first.token).unwrap_err());
        allocator.release_owned(vmid, second.token).unwrap();
        let third = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        assert_ne!(third.token, second.token);
        drop(
            allocator
                .mark_process_exited_owned(vmid, second.token)
                .unwrap_err(),
        );
        drop(allocator.release_owned(vmid, second.token).unwrap_err());
        drop(allocator.rollback(vmid, second).unwrap_err());
        drop(allocator.ensure_no_active_lease(vmid).unwrap_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rollback_restores_stopped_cid_without_reviving_old_token_authority() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        let prior = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        allocator
            .mark_process_exited_owned(vmid, prior.token)
            .unwrap();
        let receiver = allocator.reserve_with_previous(vmid, Some(47)).unwrap();
        allocator.rollback(vmid, receiver).unwrap();
        let registry = allocator.read_registry().unwrap();
        assert_eq!(registry.assignments.get(&vmid.to_string()), Some(&42));
        assert_eq!(
            registry.lease_tokens.get(&vmid.to_string()),
            Some(&receiver.token)
        );
        assert!(registry.process_exited.contains(&vmid.to_string()));
        drop(allocator.release_owned(vmid, prior.token).unwrap_err());
        allocator.release_owned(vmid, receiver.token).unwrap();
        assert!(!allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_records_require_persisted_exit_and_acquire_tokens_on_reservation() {
        let (allocator, directory) = allocator();
        fs::create_dir_all(&directory).unwrap();
        let vmid = Ulid::generate();
        let legacy = serde_json::json!({
            "version": 1,
            "assignments": { vmid.to_string(): 42 },
        });
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        drop(allocator.reserve_with_previous(vmid, None).unwrap_err());
        drop(allocator.release_stopped(vmid).unwrap_err());
        let mut stopped = legacy;
        stopped["process_exited"] = serde_json::json!([vmid.to_string()]);
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&stopped).unwrap(),
        )
        .unwrap();
        let reservation = allocator.reserve_with_previous(vmid, None).unwrap();
        assert_eq!(reservation.cid, 42);
        drop(allocator.release_stopped(vmid).unwrap_err());
        allocator.rollback(vmid, reservation).unwrap();
        assert!(allocator.release_stopped(vmid).unwrap());
        // The legacy stopped record can also be deleted without reservation.
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&stopped).unwrap(),
        )
        .unwrap();
        assert!(allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
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
