pub mod nodes;
pub mod types;
pub mod vms;
pub mod volumes;
use aide::{
    axum::{ApiRouter, IntoApiResponse, routing::get},
    openapi::{Info, OpenApi},
    swagger::Swagger,
};
use axum::{Extension, Json, Router};
use kameo::actor::ActorRef;

use crate::actors::http_actor::HTTPActor;

/// Build the full app: finalizes the OpenAPI spec and attaches it as an extension.
pub fn build(state: ActorRef<HTTPActor>) -> Router {
    aide::generate::on_error(|error| {
        tracing::warn!("aide schema gen error: {error}");
    });

    let mut openapi = OpenApi {
        info: Info {
            title: "odorobo-manager".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        },
        ..Default::default()
    };

    router::<()>(state)
        .finish_api(&mut openapi)
        .layer(Extension(openapi))
}

// todo: error handling
//
// see odorobo-agent's old API for error handling patterns,
// use `thiserror` and `axum_responses` to create consistent error responses across the API
// - cappy

/// Main router for the API
fn router<S>(state: ActorRef<HTTPActor>) -> ApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    ApiRouter::<ActorRef<HTTPActor>>::new()
        .api_route("/health", get(health))
        .api_route("/swagger", Swagger::new("/openapi.json").axum_route())
        .api_route("/openapi.json", get(serve_api))
        .nest("/nodes", nodes::router().with_state(state.clone()))
        .nest("/vms", vms::router().with_state(state.clone()))
        .nest("/volumes", volumes::router().with_state(state.clone()))
        .with_state(state)
}

/// Serve the OpenAPI spec as JSON
async fn serve_api(Extension(api): Extension<OpenApi>) -> impl IntoApiResponse {
    Json(api)
}

/// Simple health check endpoint
///
/// Returns "OK" if the server is running.
async fn health() -> &'static str {
    "OK"
}
