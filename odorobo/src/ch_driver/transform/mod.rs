use cloud_hypervisor_client::models::VmConfig;
use stable_eyre::Result;
use url::Url;

/// Result of claiming a node-local storage resource for one VM.
#[allow(clippy::struct_excessive_bools)] // These flags are independent durable facts, not a mutually exclusive state.
#[derive(Clone, Debug)]
pub struct StorageClaim {
    /// Whether this call created the claim rather than reusing this VM's claim.
    pub is_new: bool,
    /// Whether a cleanup journal entry was durably recorded for this claim.
    pub journaled: bool,
    /// Backend-specific identity persisted after acquisition (for example an
    /// iSCSI session id and numeric portal).
    pub attachment: Option<String>,
    /// This claim was created by an earlier agent process. Kernel paths and
    /// session identifiers alone cannot prove that the original attachment is
    /// still present after process recovery.
    pub recovered: bool,
    /// Release was durably marked as started before an irreversible backend
    /// operation. Backends use this to avoid retrying an operation against a
    /// potentially recycled kernel identity after a crash.
    pub release_started: bool,
}

/// Durable release state supplied to a backend before cleanup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageReleaseState {
    pub already_started: bool,
    pub recovered: bool,
    /// A matching durable ownership record exists. Legacy journals without a
    /// claim have no proof that an asynchronous backend request was not sent.
    pub claim_exists: bool,
}

/// Durable ownership callbacks used by storage transforms. Claims are acquired
/// before backend prechecks and outlive release until the cleanup journal has
/// been durably updated.
pub struct StorageOwnershipContext<'a> {
    pub claim: &'a mut (dyn FnMut(&Url, &str) -> Result<StorageClaim> + Send),
    pub record: &'a mut (dyn FnMut(&Url) -> Result<()> + Send),
    pub mark_journaled: &'a mut (dyn FnMut(&Url, &str) -> Result<()> + Send),
    pub set_attachment: &'a mut (dyn FnMut(&Url, &str, &str) -> Result<()> + Send),
    pub abandon: &'a mut (dyn FnMut(&Url, &str) -> Result<()> + Send),
}

/// Ownership metadata access during durable storage release.
pub struct StorageCleanupContext<'a> {
    pub attachment: &'a mut (dyn FnMut(&Url, &str) -> Result<Option<String>> + Send),
    /// Atomically persists release intent and reports whether the claim came
    /// from an earlier process or a prior release attempt.
    pub begin_release: &'a mut (dyn FnMut(&Url, &str) -> Result<StorageReleaseState> + Send),
    pub forget: &'a mut (dyn FnMut(&Url, &str) -> Result<()> + Send),
}

pub trait ConfigTransform: Send + Sync {
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()>;

    /// Transform while durably recording each storage resource before an
    /// operation which may acquire it. Non-storage transforms can keep using
    /// `transform`; storage transforms override this hook.
    fn transform_with_cleanup_journal(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        _record: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        self.transform(vmid, config)
    }

    /// Storage-specific transform path with a durable per-resource ownership
    /// claim. Non-storage transforms retain their existing behavior.
    fn transform_with_storage_ownership(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        ownership: &mut StorageOwnershipContext<'_>,
    ) -> Result<()> {
        self.transform_with_cleanup_journal(vmid, config, ownership.record)
    }

    /// Optional teardown method to reverse transformations if needed,
    /// used for tearing down VMs
    fn teardown(&self, _vmid: &str, _config: &mut VmConfig) -> Result<()> {
        Ok(())
    }

    /// Teardown using only resources explicitly recorded as acquired or
    /// ambiguously attempted. The callback forgets a journal entry only after
    /// its release succeeds. Legacy transforms retain their existing teardown.
    fn teardown_with_cleanup_journal(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        _journal: &[Url],
        _forget: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        self.teardown(vmid, config)
    }

    /// Validate that cleanup metadata is sufficient before teardown or purge.
    /// `journal_is_durable` distinguishes a known empty journal from metadata
    /// created before storage acquisition journaling existed.
    fn validate_storage_cleanup_metadata(
        &self,
        _vmid: &str,
        _config: &VmConfig,
        _journal_is_durable: bool,
    ) -> Result<()> {
        Ok(())
    }

    /// Teardown path that supplies the pinned attachment identity and forgets
    /// ownership only after release plus durable journal completion.
    fn teardown_with_storage_ownership(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        journal: &[Url],
        cleanup: &mut StorageCleanupContext<'_>,
    ) -> Result<()> {
        let mut forget = |uri: &Url| (cleanup.forget)(uri, "");
        self.teardown_with_cleanup_journal(vmid, config, journal, &mut forget)
    }
}

pub mod console;
pub mod networking;
pub mod path_verify;
pub mod storage;
pub use console::ConsoleTransform;
pub use networking::NetworkTransform;
pub use path_verify::PathVerify;
use tracing::trace;

pub struct TransformChain(Vec<Box<dyn ConfigTransform>>);

impl TransformChain {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn add<T: ConfigTransform + 'static>(mut self, transform: T) -> Self {
        self.0.push(Box::new(transform));
        self
    }

    pub fn then(self) -> Box<dyn ConfigTransform> {
        Box::new(self)
    }
}

impl ConfigTransform for TransformChain {
    fn transform(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        trace!("Applying transform chain with {} transforms", self.0.len());
        for t in &self.0 {
            t.transform(vmid, config)?;
        }
        Ok(())
    }

    fn transform_with_cleanup_journal(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        record: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        trace!("Applying transform chain with {} transforms", self.0.len());
        for transform in &self.0 {
            transform.transform_with_cleanup_journal(vmid, config, record)?;
        }
        Ok(())
    }

    fn transform_with_storage_ownership(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        ownership: &mut StorageOwnershipContext<'_>,
    ) -> Result<()> {
        trace!("Applying transform chain with {} transforms", self.0.len());
        for transform in &self.0 {
            transform.transform_with_storage_ownership(vmid, config, ownership)?;
        }
        Ok(())
    }

    fn validate_storage_cleanup_metadata(
        &self,
        vmid: &str,
        config: &VmConfig,
        journal_is_durable: bool,
    ) -> Result<()> {
        for transform in &self.0 {
            transform.validate_storage_cleanup_metadata(vmid, config, journal_is_durable)?;
        }
        Ok(())
    }

    fn teardown(&self, vmid: &str, config: &mut VmConfig) -> Result<()> {
        trace!("Teardown transform chain with {} transforms", self.0.len());
        for t in self.0.iter().rev() {
            t.teardown(vmid, config)?;
        }
        Ok(())
    }

    fn teardown_with_cleanup_journal(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        journal: &[Url],
        forget: &mut dyn FnMut(&Url) -> Result<()>,
    ) -> Result<()> {
        trace!("Teardown transform chain with {} transforms", self.0.len());
        for transform in self.0.iter().rev() {
            transform.teardown_with_cleanup_journal(vmid, config, journal, forget)?;
        }
        Ok(())
    }

    fn teardown_with_storage_ownership(
        &self,
        vmid: &str,
        config: &mut VmConfig,
        journal: &[Url],
        cleanup: &mut StorageCleanupContext<'_>,
    ) -> Result<()> {
        trace!("Teardown transform chain with {} transforms", self.0.len());
        for transform in self.0.iter().rev() {
            transform.teardown_with_storage_ownership(vmid, config, journal, cleanup)?;
        }
        Ok(())
    }
}

impl Default for TransformChain {
    fn default() -> Self {
        Self::new()
            .add(storage::StorageDriverTransformer::default())
            .add(ConsoleTransform)
            .add(NetworkTransform)
            .add(PathVerify)
    }
}
