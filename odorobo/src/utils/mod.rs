pub mod actor_names;
pub mod lockfile;

use aide::OperationIo;
use api_error::ApiError;
use kameo::prelude::*;
use libp2p::futures::StreamExt;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{PeerId, mdns, noise, tcp, yamux};
use stable_eyre::{Report, Result};
use thiserror::Error;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Application error returned by request handlers.
#[derive(Error, Debug, ApiError, OperationIo)]
#[aide(output)]
pub enum OdoroboError {
    #[error("{0}")]
    #[api_error(status_code = 404, message = "{0}")]
    NotFound(String),
    #[error("{0}")]
    #[api_error(status_code = 500, message = "{0}")]
    Report(#[from] Report),
}

impl<M> From<kameo::error::SendError<M, Report>> for OdoroboError {
    fn from(value: kameo::error::SendError<M, Report>) -> Self {
        let kameo_error = value.to_string();
        error!(?value);
        Self::Report(
            value.err().unwrap_or_else(|| {
                Report::msg(format!("could not unwrap kameo error: {kameo_error}"))
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, http::StatusCode, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn handler() -> Result<(), OdoroboError> {
        Err(OdoroboError::Report(Report::msg("error!")))
    }

    async fn not_found_handler() -> Result<(), OdoroboError> {
        Err(OdoroboError::NotFound("missing".to_owned()))
    }

    #[tokio::test]
    async fn test_error() {
        let response = Router::new()
            .route("/", get(handler))
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = response.into_body();
        let bytes = body.collect().await.unwrap().to_bytes();
        let html = String::from_utf8(bytes.to_vec()).unwrap();

        assert_eq!(html, "{\"message\":\"error!\"}");
    }

    #[tokio::test]
    async fn test_not_found_error() {
        let response = Router::new()
            .route("/", get(not_found_handler))
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

pub fn env_filter(debug_target: Option<&str>) -> EnvFilter {
    let env = std::env::var("ODOROBO_LOG").unwrap_or_else(|_| String::new());

    let base = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(&env);

    #[cfg(debug_assertions)]
    let base = {
        let base = if let Some(debug_target) = debug_target {
            base.add_directive(format!("{debug_target}=trace").parse().unwrap())
        } else {
            base
        };

        base.add_directive(
            format!("{}=debug", env!("CARGO_PKG_NAME").replace('-', "_"))
                .parse()
                .unwrap(),
        )
    };

    base
}

pub fn init(debug_target: Option<&str>) -> Result<()> {
    stable_eyre::install()?;
    let fmt = tracing_subscriber::fmt::layer();
    #[cfg(debug_assertions)]
    let fmt = fmt
        .pretty()
        .with_file(true)
        .with_line_number(true)
        .with_ansi(true);

    tracing_subscriber::registry()
        .with(sentry::integrations::tracing::layer())
        .with(fmt.with_filter(env_filter(debug_target)))
        .init();

    Ok(())
}

#[expect(
    dead_code,
    reason = "convenience initializer for binaries/tests that do not need a debug target"
)]
pub fn init_default() -> Result<()> {
    init(None)
}

#[derive(NetworkBehaviour)]
pub struct ProductionBehaviour {
    kameo: remote::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

// based on:
// https://github.com/tqwewe/kameo/blob/main/examples/custom_swarm.rs
// https://docs.page/tqwewe/kameo/distributed-actors/custom-swarm-configuration
pub fn connect_to_swarm() -> Result<PeerId> {
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();

            let kameo = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(std::time::Duration::from_secs(120)),
            );
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            Ok(ProductionBehaviour { kameo, mdns })
        })?
        .build();

    // Initialize Kameo's global registry
    swarm.behaviour().kameo.init_global();

    // Listen on a specific address
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    let local_peer_id = *swarm.local_peer_id();

    info!("Local peer id: {:?}", local_peer_id);

    // Spawn the swarm task
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                // Handle mDNS discovery
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Mdns(mdns::Event::Discovered(
                    list,
                ))) => {
                    for (peer_id, multiaddr) in list {
                        info!("mDNS discovered peer: {peer_id}");
                        swarm.add_peer_address(peer_id, multiaddr);
                    }
                }
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Mdns(mdns::Event::Expired(
                    list,
                ))) => {
                    for (peer_id, _) in list {
                        warn!("mDNS peer expired: {peer_id}");
                        _ = swarm.disconnect_peer_id(peer_id);
                    }
                }
                // Handle Kameo events (optional - for monitoring)
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Kameo(
                    remote::Event::Registry(registry_event),
                )) => {
                    debug!(?registry_event, "Registry event");
                }
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Kameo(
                    remote::Event::Messaging(messaging_event),
                )) => {
                    trace!(?messaging_event, "Messaging event");
                }
                // Handle other swarm events
                SwarmEvent::NewListenAddr { address, .. } => {
                    info!(?address, "Listening");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    info!("Connected to {peer_id}");
                }
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    info!("Disconnected from {peer_id}: {cause:?}");
                }
                _ => {}
            }
        }
    });

    Ok(local_peer_id)
}
