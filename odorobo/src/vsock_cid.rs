//! persistent, node-local virtio-vsock guest CID allocation

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

use crate::vmm_process::{BootId, ProcessIdentity};

const FIRST_GUEST_CID: u32 = 3;
const MAX_GUEST_CID: u32 = u32::MAX - 1;
/// bump this for any change an older agent can't read. older agents refuse a
/// newer version instead of rewriting the file without the fields they don't
/// know. 2 added the boot ID to recorded processes
const REGISTRY_VERSION: u32 = 2;
/// oldest version we can still read and upgrade in place
const OLDEST_REGISTRY_VERSION: u32 = 1;
const REGISTRY_ENV: &str = "ODOROBO_VSOCK_CID_REGISTRY";
const DEFAULT_REGISTRY: &str = "/var/lib/odorobo/vsock-cids.json";
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct VsockCidAllocator {
    registry_path: PathBuf,
    #[cfg(test)]
    sync_fault: Option<RegistrySyncFault>,
}

/// a registry update went through, but we couldn't confirm it'll survive a crash
///
/// the rename worked but syncing the directory failed, so callers can't treat
/// the registry as unchanged. for a failed reservation made before we spawn a
/// VMM, the caller can safely hang on to its confirmed-exit cleanup state.
/// normal validation or pre-publish errors don't give that guarantee, since
/// they might be about someone else's existing lease
#[derive(Debug, thiserror::Error)]
#[error("failed to sync vsock CID registry directory after publishing update: {source}")]
pub struct PublishedRegistryUpdate {
    #[source]
    pub source: std::io::Error,
    /// set for a reservation even if the directory sync failed after the rename.
    /// only its token can show that reservation's process exited, and its
    /// previous CID is what a rollback has to restore
    pub reservation: Option<VsockCidReservation>,
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

/// opaque ID for a single reservation, separate from the VM ID and CID reuse
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
    /// old registries don't have tokens, so those leases only get released once
    /// there's a stopped record on disk. every new reservation gets a fresh token
    #[serde(default)]
    lease_tokens: BTreeMap<String, VsockLeaseToken>,
    /// the VMM holding each active lease, so a restarted agent can tell whether
    /// it's still running instead of blocking the ID forever
    #[serde(default, deserialize_with = "deserialize_processes")]
    processes: BTreeMap<String, ProcessIdentity>,
}

/// just enough to check the version before the strict parse, so a newer file
/// gets a clear error instead of a complaint about fields we don't know
#[derive(Deserialize)]
struct RegistryVersion {
    version: u32,
}

/// a recorded process as any version wrote it. version 1 had no boot ID
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProcess {
    pid: u32,
    start_time: u64,
    #[serde(default)]
    boot_id: Option<BootId>,
}

/// drops processes recorded without a boot ID. we can't tell which boot they
/// came from, so they can't safely be checked or killed. their leases go back
/// to needing a manual cleanup, like before processes were recorded at all
fn deserialize_processes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, ProcessIdentity>, D::Error> {
    let stored = BTreeMap::<String, StoredProcess>::deserialize(deserializer)?;
    Ok(stored
        .into_iter()
        .filter_map(|(vmid, process)| {
            let Some(boot_id) = process.boot_id else {
                tracing::warn!(%vmid, pid = process.pid, "forgetting a VMM recorded without a boot ID");
                return None;
            };
            Some((
                vmid,
                ProcessIdentity {
                    pid: process.pid,
                    start_time: process.start_time,
                    boot_id,
                },
            ))
        })
        .collect())
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
        self.processes.remove(vmid);
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

    /// reserve a CID and keep enough info to undo it if VM startup fails. CIDs
    /// 0-2 and `u32::MAX` are reserved by the transport. if the error has a
    /// [`PublishedRegistryUpdate`] in it, this reservation did get written, and
    /// the error carries it
    pub fn reserve_with_previous(
        &self,
        vmid: Ulid,
        requested: Option<u32>,
    ) -> Result<VsockCidReservation> {
        let token = VsockLeaseToken(Ulid::generate());
        let mut made = None;
        let result = self.with_registry(|registry| {
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
            registry.processes.remove(&vmid);
            registry.lease_tokens.insert(vmid, token);
            let reservation = VsockCidReservation {
                cid,
                token,
                previous_cid: existing,
                previous_process_exited,
            };
            made = Some(reservation);
            Ok(reservation)
        });
        result.map_err(|error| match error.downcast::<PublishedRegistryUpdate>() {
            Ok(mut published) => {
                published.reservation = made;
                stable_eyre::Report::new(published)
            }
            Err(error) => error,
        })
    }

    /// put the old assignment back after a failed startup, but only if the
    /// registry still has the reservation we made
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
                // rollback only happens after our VMM exited, so its PID is stale
                registry.processes.remove(&vmid);
                // keep this reservation's token. restoring the stopped CID
                // shouldn't let an older token act on the lease again
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

    /// a new VM without vsock still can't skip past an older active lease. this
    /// only reads the registry, it never takes ownership or marks anything exited
    pub fn ensure_no_active_lease(&self, vmid: Ulid) -> Result<()> {
        let registry = self.read_registry()?;
        let key = vmid.to_string();
        if registry.assignments.contains_key(&key) && !registry.process_exited.contains(&key) {
            return Err(stable_eyre::Report::new(ActiveVsockLease(vmid)));
        }
        Ok(())
    }

    /// remember which VMM holds this reservation. a stale token can't attach a
    /// process to a newer run of the same VM
    pub fn record_process_owned(
        &self,
        vmid: Ulid,
        token: VsockLeaseToken,
        process: ProcessIdentity,
    ) -> Result<()> {
        self.with_registry(|registry| {
            let vmid = vmid.to_string();
            if registry.owns_lease(&vmid, token)? {
                registry.processes.insert(vmid, process);
            }
            Ok(())
        })
    }

    /// the VMM recorded for an active lease, if we know it
    pub fn recorded_process(&self, vmid: Ulid) -> Result<Option<ProcessIdentity>> {
        let registry = self.read_registry()?;
        let key = vmid.to_string();
        if registry.process_exited.contains(&key) {
            return Ok(None);
        }
        Ok(registry.processes.get(&key).copied())
    }

    /// mark an active lease stopped if its recorded VMM is gone. this is how a
    /// restarted agent gets past leases it lost track of. returns whether the
    /// lease is now stopped (or there wasn't an active one)
    pub fn reconcile_exited(&self, vmid: Ulid) -> Result<bool> {
        let key = vmid.to_string();
        let registry = self.read_registry()?;
        if !registry.assignments.contains_key(&key) || registry.process_exited.contains(&key) {
            return Ok(true);
        }
        let Some(process) = registry.processes.get(&key).copied() else {
            return Ok(false);
        };
        if process.is_running()? {
            return Ok(false);
        }
        self.with_registry(|registry| {
            // only trust the check if nobody swapped the lease while we weren't holding the lock
            if registry.assignments.contains_key(&key)
                && registry.processes.get(&key) == Some(&process)
            {
                registry.process_exited.insert(key.clone());
            }
            Ok(!registry.assignments.contains_key(&key) || registry.process_exited.contains(&key))
        })
    }

    /// every VM with a lease on this node, running or not
    pub fn leased_vms(&self) -> Result<Vec<Ulid>> {
        Ok(self
            .read_registry()?
            .assignments
            .keys()
            .filter_map(|vmid| vmid.parse().ok())
            .collect())
    }

    /// mark exit only for the exact reservation this VMM owns. a stale token
    /// can't mark a newer run of the same VM as exited
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

    /// release after a confirmed exit, under the same registry lock as removal.
    /// don't fall back to `release_stopped` if the token doesn't match
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

    /// unguarded helpers, only for the old allocator tests. real callers need a
    /// token or a stopped record on disk
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

    /// release a lease on disk, but only if the VM actor already recorded a
    /// confirmed process exit. returns whether there was a lease, so callers can
    /// use `false` to tell an untracked VM apart from one we actually released
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
        // just drop the file to unlock. an explicit unlock can fail, and then an
        // update that already went through would come back as an error
        drop(lock);
        Ok(result)
    }

    fn read_registry(&self) -> Result<Registry> {
        let mut registry = match fs::read(&self.registry_path) {
            Ok(contents) => {
                let version = serde_json::from_slice::<RegistryVersion>(&contents)
                    .wrap_err("vsock CID registry is invalid JSON")?
                    .version;
                if version > REGISTRY_VERSION {
                    return Err(eyre!(
                        "vsock CID registry is version {version}, written by a newer odorobo. \
                         this one only supports up to version {REGISTRY_VERSION}"
                    ));
                }
                if version < OLDEST_REGISTRY_VERSION {
                    return Err(eyre!("unsupported vsock CID registry version {version}"));
                }
                serde_json::from_slice::<Registry>(&contents)
                    .wrap_err("vsock CID registry is invalid JSON")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry {
                version: REGISTRY_VERSION,
                assignments: BTreeMap::new(),
                process_exited: BTreeSet::new(),
                lease_tokens: BTreeMap::new(),
                processes: BTreeMap::new(),
            },
            Err(error) => return Err(error).wrap_err("failed to read vsock CID registry"),
        };
        // older versions upgrade in place. the next write saves the new version
        registry.version = REGISTRY_VERSION;
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
            self.sync_temporary_registry(&file)
                .wrap_err("failed to sync temporary vsock CID registry")?;
            fs::rename(&temp_path, &self.registry_path)
                .wrap_err("failed to publish vsock CID registry")?;
            self.sync_registry_directory(parent)
                .map_err(|source| PublishedRegistryUpdate {
                    source,
                    reservation: None,
                })?;
            Ok(())
        })();
        if result.is_err() {
            drop(fs::remove_file(&temp_path));
        }
        result
    }

    // the allocator only carries per-instance fault injection in test builds
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
        // restoring an earlier CID doesn't make the earlier token valid again
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
            .reservation
            .map(|reservation| reservation.token)
            .expect("published reservation carries ownership");
        assert_eq!(registry.lease_tokens.get(&vmid.to_string()), Some(&token));

        // the failed write let go of its lock. a fresh allocator sees an active
        // lease and can only clean it up once exit is confirmed (nothing spawned)
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
            .reservation
            .map(|reservation| reservation.token)
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

        // marking a new exit doesn't let an older token delete the lease, even if
        // a retry already managed to mark its own exit
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
        // the legacy stopped record can also be deleted without a reservation
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&stopped).unwrap(),
        )
        .unwrap();
        assert!(allocator.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn version_1_registries_upgrade_and_drop_processes_without_a_boot_id() {
        let (allocator, directory) = allocator();
        fs::create_dir_all(&directory).unwrap();
        let vmid = Ulid::generate();
        let token = VsockLeaseToken(Ulid::generate());
        let legacy = serde_json::json!({
            "version": 1,
            "assignments": { vmid.to_string(): 42 },
            "lease_tokens": { vmid.to_string(): token },
            "processes": { vmid.to_string(): { "pid": 1, "start_time": 1 } },
        });
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        // we can't tell which boot that process came from, so it's not used
        assert_eq!(allocator.recorded_process(vmid).unwrap(), None);
        assert!(!allocator.reconcile_exited(vmid).unwrap());

        // the next write saves it as the current version with a boot ID
        let process = ProcessIdentity::of(std::process::id()).unwrap();
        allocator
            .record_process_owned(vmid, token, process)
            .unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&allocator.registry_path).unwrap()).unwrap();
        assert_eq!(written["version"], REGISTRY_VERSION);
        assert_eq!(
            written["processes"][vmid.to_string()]["boot_id"],
            BootId::current().unwrap().to_string()
        );
        assert_eq!(allocator.recorded_process(vmid).unwrap(), Some(process));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn newer_registry_versions_are_refused_with_a_clear_error() {
        let (allocator, directory) = allocator();
        fs::create_dir_all(&directory).unwrap();
        let newer = serde_json::json!({
            "version": REGISTRY_VERSION + 1,
            "assignments": {},
            "something_new": true,
        });
        fs::write(
            &allocator.registry_path,
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        let error = allocator.leased_vms().unwrap_err().to_string();
        assert!(error.contains("written by a newer odorobo"), "{error}");
        // and nothing gets rewritten over it
        drop(
            allocator
                .reserve_with_previous(Ulid::generate(), None)
                .unwrap_err(),
        );
        let on_disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&allocator.registry_path).unwrap()).unwrap();
        assert_eq!(on_disk, newer);
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

    #[tokio::test]
    async fn restarted_agent_reconciles_a_lease_only_once_its_vmm_is_gone() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        let reservation = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let mut vmm = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let process = ProcessIdentity::of(vmm.id().unwrap()).unwrap();
        allocator
            .record_process_owned(vmid, reservation.token, process)
            .unwrap();
        assert_eq!(allocator.leased_vms().unwrap(), vec![vmid]);

        // a fresh allocator has no token, like an agent after a restart
        let restarted = VsockCidAllocator::new(allocator.registry_path.clone());
        assert_eq!(restarted.recorded_process(vmid).unwrap(), Some(process));
        assert!(!restarted.reconcile_exited(vmid).unwrap());
        assert!(restarted.ensure_no_active_lease(vmid).is_err());

        vmm.kill().await.unwrap();
        assert!(restarted.reconcile_exited(vmid).unwrap());
        restarted.ensure_no_active_lease(vmid).unwrap();
        assert_eq!(restarted.recorded_process(vmid).unwrap(), None);
        assert!(restarted.release_stopped(vmid).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lease_without_a_recorded_vmm_stays_active() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        reserve(&allocator, vmid, Some(42)).unwrap();
        assert!(!allocator.reconcile_exited(vmid).unwrap());
        assert!(allocator.ensure_no_active_lease(vmid).is_err());
        // nothing to reconcile when there's no lease at all
        assert!(allocator.reconcile_exited(Ulid::generate()).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_token_cant_attach_a_process_and_new_reservations_forget_the_old_one() {
        let (allocator, directory) = allocator();
        let vmid = Ulid::generate();
        let old = allocator.reserve_with_previous(vmid, Some(42)).unwrap();
        let process = ProcessIdentity {
            pid: std::process::id(),
            start_time: 1,
            boot_id: BootId::current().unwrap(),
        };
        allocator
            .record_process_owned(vmid, old.token, process)
            .unwrap();
        allocator
            .mark_process_exited_owned(vmid, old.token)
            .unwrap();

        let new = allocator.reserve_with_previous(vmid, None).unwrap();
        assert_eq!(new.cid, 42);
        assert_eq!(allocator.recorded_process(vmid).unwrap(), None);
        assert!(
            allocator
                .record_process_owned(vmid, old.token, process)
                .is_err()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn published_reservation_error_carries_rollback_info() {
        let (mut allocator, directory) = allocator();
        let vmid = Ulid::generate();
        reserve(&allocator, vmid, Some(42)).unwrap();
        allocator.mark_process_exited(vmid).unwrap();
        allocator.sync_fault = Some(RegistrySyncFault::Directory);

        let error = allocator.reserve_with_previous(vmid, Some(43)).unwrap_err();
        let reservation = error
            .downcast_ref::<PublishedRegistryUpdate>()
            .and_then(|published| published.reservation)
            .expect("published reservation");
        assert_eq!(reservation.cid, 43);

        allocator.sync_fault = None;
        allocator.rollback(vmid, reservation).unwrap();
        let registry = allocator.read_registry().unwrap();
        assert_eq!(registry.assignments.get(&vmid.to_string()), Some(&42));
        assert!(registry.process_exited.contains(&vmid.to_string()));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
