//! Repository maintenance API handlers.
use crate::{
    actors::http_actor::HTTPActor,
    messages::agent::{CollectRepositoryGarbage, RepositoryGcReply},
    utils::OdoroboError,
};
use aide::axum::{ApiRouter, IntoApiResponse, routing::post};
use axum::{Json, extract::State};
use kameo::actor::ActorRef;
use schemars::JsonSchema;
use serde::Deserialize;

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new().api_route("/gc", post(collect_garbage))
}

#[derive(Deserialize, JsonSchema)]
pub struct CollectGarbageRequest {
    /// Report what would be removed without deleting anything.
    #[serde(default)]
    pub dry_run: bool,
}

/// Reclaim composefs repository space on every known node.
async fn collect_garbage(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<CollectGarbageRequest>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply: RepositoryGcReply = state
        .ask(CollectRepositoryGarbage {
            dry_run: request.dry_run,
        })
        .await?;
    let response = serde_json::to_value(reply)
        .map_err(|error| OdoroboError::Report(stable_eyre::Report::from(error)))?;

    Ok(Json(response))
}
