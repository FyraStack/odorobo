//! Temporary state management for the agent.
//!
//! Runtime state (in /run) is not persisted across reboots, so we use it for
//! ephemeral VM state like running instances. Persistent state goes in the database.

pub mod api;
pub mod devices;
pub mod instance;
pub mod provisioning;
pub mod transform;
pub mod actor;

pub use instance::{VMInstance};
