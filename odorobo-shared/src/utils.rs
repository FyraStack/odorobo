use stable_eyre::Result;
// use tracing::
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt, prelude::*};

pub fn env_filter() -> EnvFilter {
    let env = std::env::var("ODOROBO_LOG").unwrap_or_else(|_| "".into());

    let base = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(&env);

    #[cfg(debug_assertions)]
    let base = base.add_directive("odorobo*=trace".parse().unwrap());

    base
}

pub fn init() -> Result<()> {
    stable_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_file(true)
        .with_line_number(true)
        .init();

    Ok(())
}
