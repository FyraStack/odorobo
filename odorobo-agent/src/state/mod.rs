//! Temporary state management for the agent.
//!
//! Runtime state (in /run) is not persisted across reboots, so we use it for
//! ephemeral VM state like running instances. Persistent state goes in the database.

pub mod api;
pub mod instance;
pub mod devices;
pub mod provisioning;
pub mod transform;

pub use api::{call, call_request};
pub use instance::{CONFIG_FILE_NAME, ChApiError, ConsoleStream, VMInstance, VMS_DIR_NAME};
// pub use transform::{ConfigTransform, ConsoleTransform, TransformChain, apply_builtin_transforms};
