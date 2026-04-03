//! Temporary state management for the agent.
//!
//! Runtime state (in /run) is not persisted across reboots, so we use it for
//! ephemeral VM state like running instances. Persistent state goes in the database.

mod api;
mod instance;
mod devices;
mod provisioning;
mod transform;

pub use api::{call, call_request};
pub use instance::{CONFIG_FILE_NAME, ChApiError, ConsoleStream, VMInstance, VMS_DIR_NAME};
// pub use transform::{ConfigTransform, ConsoleTransform, TransformChain, apply_builtin_transforms};
