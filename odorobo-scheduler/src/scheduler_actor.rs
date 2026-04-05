use axum::{Json, Router};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use kameo::prelude::*;
use libp2p::futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use libp2p::PeerId;
use utoipa::OpenApi;
use odorobo_shared::kameo_messages::{ServerStatus, GetServerStatus};
//use odorobo_shared::odorobo::server_actor::ServerActor;
use odorobo_shared::utils::DynError;

#[derive(RemoteActor)]
pub struct SchedulerActor { }

const PING_RETURN_VALUE: &str = "pong";
const EXTERNAL_HTTP_ADDRESS: &str = "0.0.0.0:3000";

const EXTERNAL_HTTP_URL: &str = "http://localhost:3000"; // TODO: make this based on EXTERNAL_HTTP_ADDRESS. const compile time stuff is a pain.


impl Actor for SchedulerActor {
    type Args = Self;
    type Error = DynError;

    async fn on_start(state: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let axum_router = Router::new()
            .route("/ping", get(|| async { PING_RETURN_VALUE }))
            .route("/create_vm", post(create_vm))
            .route("/delete_vm", post(delete_vm))
            .route("/update_vm", post(update_vm))
            .route("/get_vm", post(get_vm))
            .route("/drain_server", post(drain_server))
            .route("/get_servers", post(get_servers))
            .with_state(actor_ref);


        println!("starting axum server at {}", EXTERNAL_HTTP_URL);

        // run our app with hyper, listening globally on port 3000
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(EXTERNAL_HTTP_ADDRESS).await.unwrap();
            axum::serve(listener, axum_router).await.unwrap();
        });

        // spin loop until the axum server starts responding to requests
        // TODO: if anyone has a better way to detect the axum server is up, change it to that.

        let mut count = 0;
        loop {
            count += 1;
            println!("attempting to hit axum server, attempt {}", count);

            let resp_result: Result<(), DynError> = async {
                let resp = reqwest::get(EXTERNAL_HTTP_URL.to_owned() + "/ping")
                    .await?
                    .text()
                    .await?;

                if resp != PING_RETURN_VALUE {
                    return Err("invalid ping response".into());
                }

                Ok(())
            }.await;

            match resp_result {
                Ok(()) => {
                    break;
                },
                Err(e) => {
                    println!("{}", e)
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        println!("Actor started");
        Ok(state)
    }
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct CreateVM {
    pub uuid: Uuid,
    pub name: String,
    pub vcpus: u32,
    pub ram: u32,
    pub image: String,
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct GenericSuccessResponse {
    pub success: bool,
}

// no response. just use status code 200 vs not 200 for if it worked.
#[utoipa::path(
    post,
    path = "/create_vm",
    request_body(content = CreateVM, content_type = "application/json"),
    responses(
        (status = 200, body = GenericSuccessResponse)
    )
)]
pub async fn create_vm(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<CreateVM>) -> (StatusCode, String) {
    todo!()
}

pub type UpdateVM = CreateVM;

#[utoipa::path(
    post,
    path = "/update_vm",
    request_body(content = UpdateVM, content_type = "application/json"),
    responses(
        (status = 200, body = GenericSuccessResponse)
    )
)]
async fn update_vm(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<UpdateVM>) -> (StatusCode, String) {
    todo!()
}


pub type DeleteVM = GetVM;
#[utoipa::path(
    post,
    path = "/delete_vm",
    request_body(content = DeleteVM, content_type = "application/json"),
    responses(
        (status = 200, body = GenericSuccessResponse)
    )
)]
async fn delete_vm(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<DeleteVM>) -> (StatusCode, String) {
    todo!()
}


#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct GetVM {
    pub uuid: Uuid,
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct GetVMResponse {
    pub cpus: bool
}
#[utoipa::path(
    post,
    path = "/get_vm",
    request_body(content = GetVM, content_type = "application/json"),
    responses(
        (status = 200, body = GetVMResponse)
    )
)]
async fn get_vm(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<GetVM>) -> (StatusCode, String) {
    todo!()
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct DrainServer {}

#[utoipa::path(
    post,
    path = "/get_vm",
    request_body(content = DrainServer, content_type = "application/json"),
    responses(
        (status = 200, body = GenericSuccessResponse)
    )
)]
async fn drain_server(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<DrainServer>) -> (StatusCode, String) {
    todo!()
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct GetServers {
    pub start_index: u64,
    pub end_index: u64
}

#[derive(Serialize, Deserialize, Debug, utoipa::ToSchema)]
pub struct GetServersResponse {
    pub total_servers: u64,
    pub servers: Vec<ServerStatus>,
}


#[utoipa::path(
    post,
    path = "/get_servers",
    request_body(content = GetServers, content_type = "application/json"),
    responses(
        (status = 200, body = GetServersResponse)
    )
)]
async fn get_servers(State(state): State<ActorRef<SchedulerActor>>, Json(payload): Json<GetServers>) -> (StatusCode, String) {
    /*
    let mut servers: Vec<(PeerId, ServerStatus)> = Vec::new();

    let server_actor_response: Result<(), DynError> = async {
        println!("getting server actors");

        let mut server_actors = RemoteActorRef::<ServerActor>::lookup_all("server");

        while let Some(server_actor) = server_actors.try_next().await? {
            // Send message to each instance
            println!("asking {:?}", server_actor);
            let result = server_actor.ask(&GetServerStatus { }).await?;
            println!("result {:?}", result);

            if let Some(peerId) = server_actor.id().peer_id() {
                servers.push((*peerId, result));
            }
        }

        Ok(())
    }.await;

    match server_actor_response {
        Ok(()) => {
            (StatusCode::OK, serde_json::to_string(&servers).unwrap())
        },
        _ => {
            (StatusCode::INTERNAL_SERVER_ERROR, "".parse().unwrap())
        }
    }

     */
    todo!()
}

pub fn gen_openapi_spec() -> String {
    #[derive(OpenApi)]
    #[openapi(
        components(
            schemas(CreateVM, UpdateVM, DeleteVM, GetVM, DrainServer, GetServers, GenericSuccessResponse, GetVMResponse, GetServersResponse)
        ),
        paths(get_servers)
    )]
    struct ApiDoc;

    ApiDoc::openapi().to_pretty_json().unwrap()
}