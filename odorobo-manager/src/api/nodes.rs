//! Compute node management API handlers.//! VM management API handlers.
use crate::api::types::{Node, VmId};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{get, put},
};
use axum::{Json, extract::Path};
pub fn router() -> ApiRouter {
    ApiRouter::new()
        .api_route("/drain", put(drain))
        .api_route("/{nodeid}", get(node_info))
}
/// Drain a node of all VMs, migrating them away or shutting them down as needed. This is used for maintenance mode.
async fn drain() -> impl IntoApiResponse {
    // stub
    Json("Draining...".to_string())
}

/// Get detailed information about a specific node, including its current VMs and resource usage.
async fn node_info(Path(node_id): Path<String>) -> impl IntoApiResponse {
    // stub,
    Json(Node::default())
}
