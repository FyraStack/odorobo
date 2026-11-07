//! Cloud Hypervisor-backed VM runtime.
//!
//! The agent supplies provider-neutral manifests; this module owns conversion
//! into Cloud Hypervisor configuration and all provider-specific runtime work.

pub mod actor;
pub mod api;
pub mod containers;
pub mod devices;
pub mod instance;
pub mod manifest;
pub mod provisioning;
pub mod transform;

pub use instance::VMInstance;

/// UNIX socket carrying kernel and init output from the serial console.
pub const CONSOLE_SOCKET_FILE_NAME: &str = "console.sock";

/// UNIX socket carrying the workload's stdin, stdout and stderr from the
/// virtio console.
pub const WORKLOAD_CONSOLE_SOCKET_FILE_NAME: &str = "workload.sock";
