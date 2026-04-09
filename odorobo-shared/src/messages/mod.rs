use kameo::prelude::*;
use serde::{Deserialize, Serialize};

pub mod create_vm;
pub mod debug;
pub mod server_status;

/// A request to ping the server (keepalive)
#[derive(Serialize, Deserialize, Debug)]
pub struct Ping;

/// Reply to a [`Ping`] request.
#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct Pong;
