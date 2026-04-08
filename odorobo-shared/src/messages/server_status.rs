use serde::{Deserialize, Serialize};
use kameo::Reply;

#[derive(Serialize, Deserialize)]
pub struct GetServerStatus;

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct ServerStatus {
    pub vcpus: u32,
    pub ram: u32
}