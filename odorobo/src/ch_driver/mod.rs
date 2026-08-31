//! Cloud Hypervisor-backed VM runtime.
//!
//! The agent supplies provider-neutral manifests; this module owns conversion
//! into Cloud Hypervisor configuration and all provider-specific runtime work.

pub mod actor;
pub mod api;
pub mod devices;
pub mod instance;
pub mod manifest;
pub mod provisioning;
pub mod transform;

pub use instance::VMInstance;
