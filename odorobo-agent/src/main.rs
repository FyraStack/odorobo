mod api;
mod state;
mod util;
use stable_eyre::Result;
#[tokio::main]
async fn main() -> Result<()> {
    stable_eyre::install()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_file(true)
        .with_line_number(true)
        .init();

    tracing::info!("Starting odorobo-agent...");

    // minimal axum server

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8890").await?;
    let port = listener.local_addr()?.port();
    let addrs: Vec<String> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter(|i| !i.is_loopback())
        .map(|i| format!("http://{}:{}", i.ip(), port))
        .collect();
    tracing::info!(port, ?addrs, "Listening");
    axum::serve(listener, api::router(port)).await?;

    Ok(())
}
