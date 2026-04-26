//! Volume management API handlers.
use crate::{
    actors::http_actor::HTTPActor,
    types::{CreateVolumeRequest, VolumeId, VolumeInfo}, utils::OdoroboError,
};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{delete, get, patch, put},
};
use axum::{
    Json,
    extract::{Path, State},
};
use kameo::actor::ActorRef;

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new()
        .api_route("/", put(create_volume))
        .api_route("/{volid}", get(volume_info))
        .api_route("/{volid}", delete(delete_volume))
        .api_route("/{volid}", patch(resize_volume))
}

/// Get detailed information about a specific volume
async fn volume_info(Path(VolumeId(_volid)): Path<VolumeId>) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub,
    Ok(Json(VolumeInfo::default()))
}

/// Create a new volume with the specified parameters
async fn create_volume(
    State(_state): State<ActorRef<HTTPActor>>,
    Json(_request): Json<CreateVolumeRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub
    Ok(Json(VolumeInfo::default()))
}
/// Delete an existing volume by ID
async fn delete_volume(
    State(_state): State<ActorRef<HTTPActor>>,
    Path(VolumeId(_volid)): Path<VolumeId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub
    Ok(Json(()))
}

/// Resize an existing volume to a new size
async fn resize_volume(
    State(_state): State<ActorRef<HTTPActor>>,
    Path(VolumeId(_volid)): Path<VolumeId>,
    Json(_request): Json<CreateVolumeRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    // stub
    Ok(Json(VolumeInfo::default()))
}
