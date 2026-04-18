use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};


#[derive(Serialize, Deserialize)]
pub struct GetAgentStatus;

#[derive(Serialize, Deserialize, Reply, Debug)]
pub struct AgentStatus {
    pub hostname: String,
    pub vcpus: u32,
    pub ram: ByteSize,
}
