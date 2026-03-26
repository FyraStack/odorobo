
mod state;
use cloud_hypervisor_client::{
    apis::DefaultApi,
    models::{CpusConfig, VmConfig},
};
use stable_eyre::Result;
use state::VMStateManager;
#[tokio::main]
async fn main() -> Result<()> {
    stable_eyre::install()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    state::init()?;
    tracing::info!("Starting odorobo-agent...");

    // println!("Hello, world!");

    let testvm = state::VMInstance::create_instance("testvm", None).await?;

    let list = state::VMInstance::list()?;

    tracing::info!("VM Instances: {list:#?}");

    // let info = testvm
    //     .apply_config(&VmConfig {
    //         cpus: Some(CpusConfig {
    //             boot_vcpus: 2,
    //             max_vcpus: 4,
    //             topology: None,
    //             ..Default::default()
    //         }),
    //         ..Default::default()
    //     })
    //     .await?;
    // tracing::info!(?info);

    let vminfo = testvm.info().await?;

    testvm.vmm_ping_get().await?;

    tracing::info!(?vminfo);

    Ok(())
}
