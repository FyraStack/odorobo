//! Compute node management API handlers.
use crate::{actors::http_actor::HTTPActor, types::Node, utils::OdoroboError};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{get, put},
};
use axum::{
    Json,
    extract::{Path, State},
};
use kameo::actor::ActorRef;

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new()
        .api_route("/drain", put(drain))
        .api_route("/{nodeid}", get(node_info))
}
/// Drain a node of all VMs, migrating them away or shutting them down as needed. This is used for maintenance mode.
async fn drain(State(_state): State<ActorRef<HTTPActor>>) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub
    Ok(Json("Draining...".to_string()))
}

/// Get detailed information about a specific node, including its current VMs and resource usage.
async fn node_info(
    State(_state): State<ActorRef<HTTPActor>>,
    Path(_node_id): Path<String>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub,
    Ok(Json(Node::default()))
}
