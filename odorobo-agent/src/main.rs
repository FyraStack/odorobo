pub mod actor;
mod api;
pub mod networking;
mod state;
mod util;
use kameo::actor::Spawn;
use odorobo_shared::connect_to_swarm;
use stable_eyre::Result;
// use odorobo_shared::
use crate::actor::AgentActor;

#[tokio::main]
async fn main() -> Result<()> {
    odorobo_shared::utils::init(Some("odorobo_agent"))?;

    tracing::info!("Starting odorobo-agent...");

    // minimal axum server, debug socket
    //
    // todo: remove this, here to stub out dead code
    // tokio::task::spawn(async {
    //     let listener = tokio::net::TcpListener::bind("0.0.0.0:8890").await?;
    //     let port = listener.local_addr()?.port();
    //     let addrs: Vec<String> = if_addrs::get_if_addrs()?
    //         .into_iter()
    //         .filter(|i| !i.is_loopback())
    //         .map(|i| format!("http://{}:{}", i.ip(), port))
    //         .collect();
    //     tracing::info!(port, ?addrs, "Listening");
    //     axum::serve(listener, api::router(port)).await?;
    //     Ok::<(), stable_eyre::Report>(())
    // });

    let local_peer_id = connect_to_swarm().unwrap();
    tracing::info!(?local_peer_id, "Peer ID");

    let actor_ref = AgentActor::spawn(());
    actor_ref.register("agent").await?;

    actor_ref.wait_for_shutdown().await;

    Ok(())
}
