//! VM management API handlers.
use crate::messages::vm::{AgentListVMs, DeleteVM, GetConsoleHistory, GetVMInfo, ShutdownVM};
use crate::{
    actors::http_actor::HTTPActor,
    messages::vm::CreateVM,
    types::{CreateVMRequest, VMListResponse, VmId},
    utils::OdoroboError,
};
use aide::{
    OperationInput,
    axum::{
        ApiRouter, IntoApiResponse,
        routing::{delete, get, patch, post, put},
    },
    generate::GenContext,
    openapi::Operation,
    operation::{ParamLocation, add_parameters, parameters_from_schema},
};
use axum::{
    Json,
    extract::{FromRequestParts, Path, Query, State},
    http::{header, request::Parts},
    response::IntoResponse,
};
use kameo::actor::ActorRef;
use schemars::JsonSchema;
use serde::Deserialize;

pub fn router() -> ApiRouter<ActorRef<HTTPActor>> {
    ApiRouter::new()
        .api_route("/", get(list_vms))
        .api_route("/", post(create_vm))
        .api_route("/{vmid}", get(vm_info))
        .api_route("/{vmid}", patch(update_vm))
        .api_route("/{vmid}", delete(delete_vm))
        .api_route("/{vmid}/shutdown", put(shutdown_vm))
        .api_route("/{vmid}/console/history", get(console_history))
}

async fn list_vms(
    State(state): State<ActorRef<HTTPActor>>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(AgentListVMs).await?;

    Ok(Json(VMListResponse {
        vms: reply.vms.into_iter().map(VmId).collect(),
    }))
}

/// Get detailed information about a specific VM
async fn vm_info(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(GetVMInfo { vmid: Some(vmid) }).await?;
    if reply.config.is_none() {
        return Err(OdoroboError::NotFound(format!("VM {vmid} not found")));
    }
    let response = serde_json::to_value(reply)
        .map_err(|error| OdoroboError::Report(stable_eyre::Report::from(error)))?;

    Ok(Json(response))
}

async fn create_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Json(request): Json<CreateVMRequest>,
) -> Result<Json<serde_json::Value>, OdoroboError> {
    let message = CreateVM {
        vmid: request.vm.id,
        config: request.vm,
    };

    let reply = state.ask(message).await?;

    let response = serde_json::to_value(reply)
        .map_err(|error| OdoroboError::Report(stable_eyre::Report::from(error)))?;
    Ok(Json(response))
}

/// query params for `DELETE /vms/{vmid}`
#[derive(Deserialize, JsonSchema, Debug, Default)]
struct DeleteVmQuery {
    /// give up on cleanup owners whose agent can't be reached instead of
    /// failing the delete. an agent that answers with an error still fails it.
    /// whatever an unreachable node still holds is leaked until it comes back
    #[serde(default)]
    force: bool,
}

/// `Query<DeleteVmQuery>` with docs. aide only documents `Query` with its
/// `axum-query` feature, so this does the same thing by hand
struct DeleteVmParams(DeleteVmQuery);

impl<S: Send + Sync> FromRequestParts<S> for DeleteVmParams {
    type Rejection = <Query<DeleteVmQuery> as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(query) = Query::from_request_parts(parts, state).await?;
        Ok(Self(query))
    }
}

impl OperationInput for DeleteVmParams {
    fn operation_input(ctx: &mut GenContext, operation: &mut Operation) {
        let schema = ctx.schema.subschema_for::<DeleteVmQuery>();
        let params = parameters_from_schema(ctx, schema, ParamLocation::Query);
        add_parameters(ctx, operation, params);
    }
}

async fn delete_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
    DeleteVmParams(query): DeleteVmParams,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let _reply = state
        .ask(DeleteVM {
            vmid,
            force: query.force,
        })
        .await?;

    Ok(Json(()))
}

async fn shutdown_vm(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let _reply = state.ask(ShutdownVM { vmid }).await?;

    Ok(Json(()))
}

async fn console_history(
    State(state): State<ActorRef<HTTPActor>>,
    Path(VmId(vmid)): Path<VmId>,
) -> Result<impl IntoApiResponse, OdoroboError> {
    let reply = state.ask(GetConsoleHistory { vmid }).await?;

    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        reply.history,
    ))
}

/// VM configuration updates are not supported by the scheduler yet.
async fn update_vm(Path(VmId(_vmid)): Path<VmId>) -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "message": "VM configuration updates are not supported"
        })),
    )
        .into_response()
}
