use std::fmt::{Display, Formatter};
use kameo::Reply;
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct GetServerStatus {}

#[derive(Serialize, Deserialize, Reply, Debug, utoipa::ToSchema)]
pub struct ServerStatus {
    pub vcpus: u32,
    pub ram: u32
}