use clap::Parser;

mod cli;

#[tokio::main]
async fn main() -> stable_eyre::Result<()> {
    let cli = cli::Cli::parse();
    cli::run_command(cli).await
}
