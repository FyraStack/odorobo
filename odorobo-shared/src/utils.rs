use stable_eyre::Result;
// use tracing::
use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt, prelude::*};
use ulid::Ulid;

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

pub fn vm_actor_id(vmid: Ulid) -> String {
    format!("vm:{}", vmid)
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
