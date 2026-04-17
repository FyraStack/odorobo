use ulid::Ulid;

/// Ensures the agent-global networking state is present and configured.
///
/// This is intended for local actor-to-actor use inside the agent process and
/// should not be exposed as a remote/shared message.
#[derive(Debug, Clone, Default)]
pub struct EnsureHostNetwork;

/// Attaches a Cloud Hypervisor-created TAP device to the agent-managed network.
///
/// The TAP device is expected to already exist by the time this message is sent,
/// typically from a post-boot provisioning hook once runtime interface details
/// are known.
#[derive(Debug, Clone)]
pub struct AttachTap {
    /// VM identifier for logging and future bookkeeping.
    pub vmid: Ulid,
    /// Host TAP device name created for this VM by Cloud Hypervisor.
    pub tap_name: String,
}

/// Detaches a TAP device from the agent-managed network.
///
/// This is optional for now, but defining it up front keeps the local message
/// API stable when stop-time cleanup is added later.
#[derive(Debug, Clone)]
pub struct DetachTap {
    /// VM identifier for logging and future bookkeeping.
    pub vmid: Ulid,
    /// Host TAP device name previously attached for this VM.
    pub tap_name: String,
}

/// Requests a snapshot of the local networking actor state.
#[derive(Debug, Clone, Default)]
pub struct Status;

/// Local networking status snapshot returned by the networking actor.
#[derive(Debug, Clone, Default)]
pub struct NetworkStatus {
    /// Whether host-global networking initialization has completed successfully.
    pub initialized: bool,
    /// Configured bridge device managed by the actor, if any.
    pub bridge: Option<String>,
    /// TAP devices currently tracked as attached to the managed bridge.
    pub attached_taps: Vec<AttachedTap>,
}

/// Bookkeeping entry for an attached TAP device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedTap {
    /// VM identifier associated with the TAP.
    pub vmid: Ulid,
    /// Host TAP device name.
    pub tap_name: String,
}
