use kameo::Reply;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct GetServerStatus;

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct ServerStatus {
    pub vcpus: u32,
    pub ram: u32,
}
