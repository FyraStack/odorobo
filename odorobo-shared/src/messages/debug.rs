use serde::{Deserialize, Serialize};

/// Forcibly panics the agent
///
/// Used for debugging purposes.
///
/// This should not be used in production.
#[derive(Serialize, Deserialize, Debug)]
pub struct PanicAgent;
