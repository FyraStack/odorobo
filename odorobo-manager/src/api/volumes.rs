//! Volume management API handlers.
use crate::api::types::{CreateVolumeRequest, Volume, VolumeId, VolumeInfo};
use aide::axum::{
    ApiRouter, IntoApiResponse,
    routing::{delete, get, patch, put},
};
use axum::{Json, extract::Path};

pub fn router() -> ApiRouter {
    ApiRouter::new()
        .api_route("/", put(create_volume))
        .api_route("/{volid}", get(volume_info))
        .api_route("/{volid}", delete(delete_volume))
        .api_route("/{volid}", patch(resize_volume))
}

/// Get detailed information about a specific volume
async fn volume_info(Path(VolumeId(volid)): Path<VolumeId>) -> impl IntoApiResponse {
    // stub,
    Json(VolumeInfo::default())
}

/// Create a new volume with the specified parameters
async fn create_volume(Json(request): Json<CreateVolumeRequest>) -> impl IntoApiResponse {
    // stub
    Json(VolumeInfo::default())
}
/// Delete an existing volume by ID
async fn delete_volume(Path(VolumeId(volid)): Path<VolumeId>) -> impl IntoApiResponse {
    // stub
}

/// Resize an existing volume to a new size
async fn resize_volume(
    Path(VolumeId(volid)): Path<VolumeId>,
    Json(request): Json<CreateVolumeRequest>,
) -> impl IntoApiResponse {
    // stub
    Json(VolumeInfo::default())
}
