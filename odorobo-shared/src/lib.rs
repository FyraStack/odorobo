pub mod utils;
pub mod kameo_messages;

use kameo::prelude::*;
use libp2p::{mdns, noise, tcp, yamux, PeerId};
use libp2p::futures::StreamExt;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use crate::utils::DynError;

#[derive(NetworkBehaviour)]
pub struct ProductionBehaviour {
    kameo: remote::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

// based on:
// https://github.com/tqwewe/kameo/blob/main/examples/custom_swarm.rs
// https://docs.page/tqwewe/kameo/distributed-actors/custom-swarm-configuration
pub fn connect_to_swarm() -> Result<PeerId, DynError> {
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();

            let kameo = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default(),
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

    println!("Local peer id: {:?}", local_peer_id);

    // Spawn the swarm task
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                // Handle mDNS discovery
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, multiaddr) in list {
                        println!("mDNS discovered peer: {peer_id}");
                        swarm.add_peer_address(peer_id, multiaddr);
                    }
                }
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _) in list {
                        println!("mDNS peer expired: {peer_id}");
                        let _ = swarm.disconnect_peer_id(peer_id);
                    }
                }
                // Handle Kameo events (optional - for monitoring)
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Kameo(remote::Event::Registry(
                                                                          registry_event,
                                                                      ))) => {
                    println!("Registry event: {:?}", registry_event);
                }
                SwarmEvent::Behaviour(ProductionBehaviourEvent::Kameo(remote::Event::Messaging(
                                                                          messaging_event,
                                                                      ))) => {
                    println!("Messaging event: {:?}", messaging_event);
                }
                // Handle other swarm events
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Listening on {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    println!("Connected to {peer_id}");
                }
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    println!("Disconnected from {peer_id}: {cause:?}");
                }
                _ => {}
            }
        }
    });

    Ok(local_peer_id)
}