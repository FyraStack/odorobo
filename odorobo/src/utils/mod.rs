pub mod actor_names;
pub mod actor_cache;

use stable_eyre::{Result};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use thiserror::Error;
use kameo::prelude::*;
use libp2p::futures::StreamExt;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{PeerId, mdns, noise, tcp, yamux};
use tracing::{debug, info, trace, warn};

#[derive(Error, Debug)]
pub enum OdoroboError {
    #[error("{0}")]
    Report(#[from] stable_eyre::Report),
}



pub fn env_filter(debug_target: Option<&str>) -> EnvFilter {
    let env = std::env::var("ODOROBO_LOG").unwrap_or_else(|_| "".into());

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
    let fmt = tracing_subscriber::fmt().with_env_filter(env_filter(debug_target));
    #[cfg(debug_assertions)]
    let fmt = {
        fmt.pretty()
            .with_file(true)
            .with_line_number(true)
            .with_ansi(true)
    };

    fmt.init();

    Ok(())
}

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

            let kameo = remote::Behaviour::new(local_peer_id, remote::messaging::Config::default());
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
                        let _ = swarm.disconnect_peer_id(peer_id);
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
