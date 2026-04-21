use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};
use ulid::Ulid;


#[derive(Serialize, Deserialize)]
pub struct GetAgentStatus;

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct AgentStatus {
    pub hostname: String,
    pub vcpus: u32,
    pub ram: ByteSize,
    pub vms: Vec<Ulid>
}
