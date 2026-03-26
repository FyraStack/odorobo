mod state;

use stable_eyre::Result;
use state::VMInstance;
#[tokio::main]
async fn main() -> Result<()> {
    stable_eyre::install()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    tracing::info!("Starting odorobo-agent...");

    let list = VMInstance::list()?;
    tracing::info!("VM Instances: {list:#?}");

    Ok(())
}
