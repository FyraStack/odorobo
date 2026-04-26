use bytesize::ByteSize;
use kameo::Reply;
use serde::{Deserialize, Serialize};
use ulid::Ulid;


#[derive(Serialize, Deserialize)]
pub struct GetAgentStatus;

#[derive(Serialize, Deserialize, Reply, Debug, Clone)]
pub struct AgentStatus {
    pub hostname: String,
    // todo: do we want to worry about things like CCX on epic chips? likely not necessary day 1 given we don't have epics.
    /// Total number of vcpus before over-provisionment.
    pub vcpus: u32,
    pub ram: ByteSize,
    pub used_vcpus: u32,
    pub used_ram: ByteSize,
    pub vms: Vec<Ulid>
}
