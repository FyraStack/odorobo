use axum::routing::get;

use crate::actors::http_actor::EXTERNAL_HTTP_ADDRESS;

/// HTTP server that redirects all requests to the manager.
///
/// See #154: Slave nodes run HTTP service redirecting to manager.
pub async fn relay_main_loop() {
    let listener = tokio::net::TcpListener::bind(EXTERNAL_HTTP_ADDRESS)
        .await
        .unwrap();

    tracing::info!(?EXTERNAL_HTTP_ADDRESS, "starting http relay");
    axum::serve(listener, build(get_http_actor().await))
        .await
        .unwrap();
}

type ActorRefHTTPActor = kameo::actor::RemoteActorRef<crate::actors::http_actor::HTTPActor>;

async fn get_http_actor() -> ActorRefHTTPActor {
    loop {
        match ActorRefHTTPActor::lookup(crate::utils::actor_names::HTTP_API_SERVER).await {
            Ok(Some(manager)) => break manager,
            Ok(None) => tracing::warn!("manager not yet discoverable, retrying"),
            Err(e) => tracing::warn!(?e, "manager lookup failed, retrying"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// NOTE: this is currently mostly copied from src/http_api/mod.rs
// please refactor this if you have time

/// Build the full app: finalizes the OpenAPI spec and attaches it as an extension.
pub fn build(state: ActorRefHTTPActor) -> axum::Router {
    aide::generate::on_error(|error| {
        tracing::warn!("aide schema gen error: {error}");
    });

    let mut openapi = aide::openapi::OpenApi {
        info: aide::openapi::Info {
            title: "odorobo-manager".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Default::default()
        },
        ..Default::default()
    };

    router::<()>(state)
        .finish_api(&mut openapi)
        .layer(axum::Extension(openapi))
}

/// Main router for the API
fn router<S>(state: ActorRefHTTPActor) -> aide::axum::ApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    aide::axum::ApiRouter::<ActorRefHTTPActor>::new()
        .api_route("/health", get(health).into())
        .api_route(
            "/swagger",
            aide::swagger::Swagger::new("/openapi.json").axum_route(),
        )
        .api_route("/openapi.json", get(serve_api).into())
        .nest(
            "/nodes",
            crate::http_api::nodes::router().with_state(state.clone()),
        )
        .nest(
            "/vms",
            crate::http_api::vms::router().with_state(state.clone()),
        )
        .nest(
            "/volumes",
            crate::http_api::volumes::router().with_state(state.clone()),
        )
        .with_state(state)
}

/// Serve the OpenAPI spec as JSON
async fn serve_api(
    axum::Extension(api): axum::Extension<aide::openapi::OpenApi>,
) -> Result<impl aide::axum::IntoApiResponse, crate::utils::OdoroboError> {
    Ok(axum::Json(api))
}

/// Simple health check endpoint
///
/// Returns "OK" if the server is running.
async fn health() -> &'static str {
    "OK"
}
