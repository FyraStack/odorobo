mod api;
mod state;
mod util;
use stable_eyre::Result;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

fn env_filter() -> EnvFilter {
    let env = std::env::var("ODOROBO_LOG").unwrap_or_else(|_| "".into());

    let base = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(&env);

    #[cfg(debug_assertions)]
    let base = base.add_directive("odorobo_agent=trace".parse().unwrap());

    base
}

#[tokio::main]
async fn main() -> Result<()> {
    stable_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
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
