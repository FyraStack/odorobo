mod api;
mod state;
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
    tracing::info!("Listening on http://{}", listener.local_addr()?);
    axum::serve(listener, api::router()).await?;

    Ok(())
}
