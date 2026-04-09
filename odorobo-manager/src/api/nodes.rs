//! Compute node management API handlers.
use crate::{actors::http_actor::HTTPActor, api::types::Node};
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
async fn drain(State(state): State<ActorRef<HTTPActor>>) -> impl IntoApiResponse {
    // stub
    Json("Draining...".to_string())
}

/// Get detailed information about a specific node, including its current VMs and resource usage.
async fn node_info(
    State(state): State<ActorRef<HTTPActor>>,
    Path(node_id): Path<String>,
) -> impl IntoApiResponse {
    // stub,
    Json(Node::default())
}
