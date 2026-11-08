//! Network object API handlers.
use crate::messages::network::{CreateNetwork, DeleteNetwork, GetNetwork, ListNetworks};
use crate::{
    actors::http_actor::HTTPActor,
    types::{CreateNetworkRequest, NetworkId},
    utils::OdoroboError,
};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{delete, get, post},
};
use axum::{
    Json,
    extract::{Path, State},
};
use kameo::actor::ActorRef;

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new()
        .api_route("/", get(list_networks))
        .api_route("/", post(create_network))
        .api_route("/{id}", get(network_info))
        .api_route("/{id}", delete(delete_network))
}

async fn list_networks(
    State(state): State<ActorRef<HTTPActor>>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(ListNetworks).await?;

    Ok(Json(reply))
}

/// Create a network object
///
/// Creates the network on every known agent. A network with the same ID must
/// not exist yet. The registry is in-memory, so objects do not survive a
/// manager restart.
async fn create_network(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<CreateNetworkRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let network = request.network;

    let reply = state
        .ask(CreateNetwork {
            network: network.clone(),
        })
        .await?;
    if let Some(error) = reply.error {
        return Err(OdoroboError::BadRequest(error));
    }

    Ok(Json(network))
}

/// Get a network object
async fn network_info(
    State(state): State<ActorRef<HTTPActor>>,
    Path(NetworkId(id)): Path<NetworkId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(GetNetwork { id: id.clone() }).await?;
    let Some(network) = reply.network else {
        return Err(OdoroboError::NotFound(format!("network {id} not found")));
    };

    Ok(Json(network))
}

/// Delete a network object
///
/// Fails while any VM still has a NIC on the network.
async fn delete_network(
    State(state): State<ActorRef<HTTPActor>>,
    Path(NetworkId(id)): Path<NetworkId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(DeleteNetwork { id }).await?;
    if let Some(error) = reply.error {
        return Err(OdoroboError::BadRequest(error));
    }

    Ok(Json(()))
}
