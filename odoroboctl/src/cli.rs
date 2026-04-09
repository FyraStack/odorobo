use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use stable_eyre::Result;

#[derive(Parser)]
#[command(
    name = "odoroboctl",
    about = "Command-line interface for odorobo manager"
)]
pub struct Cli {
    /// Address of the odorobo manager scheduler API server, e.g. "http://localhost:3000"
    #[arg(
        long,
        env = "ODOROBO_MANAGER_ADDR",
        default_value = "http://localhost:3000"
    )]
    pub manager_addr: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a VM via the scheduler debug endpoint,
    /// optionally also booting it immediately after creation (if `--boot` is specified).
    Create {
        /// Path to the VM config file
        /// (in Cloud Hypervisor JSON format)
        config: PathBuf,

        /// Boot the VM after creation
        #[arg(long)]
        boot: bool,
    },
}

#[derive(Debug, Serialize)]
struct DebugCreateVMRequest {
    vm_config: serde_json::Value,
    boot: bool,
}

// the fields are used using debug printing, so we allow dead code warnings
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct APIError {
    pub code: u16,
    pub message: String,
    pub errors: Option<Vec<String>>,
    pub success: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

async fn print_api_error(response: Response) -> Result<()> {
    let status = response.status();
    let body = response.text().await?;

    if let Ok(error) = serde_json::from_str::<APIError>(&body) {
        eprintln!("Error (HTTP {}): {:#?}", status.as_u16(), error);
    } else {
        eprintln!("Error (HTTP {}): {:?}", status.as_u16(), body);
    }

    Ok(())
}

async fn print_message_response(response: Response, success_message: &str) -> Result<()> {
    if response.status().is_success() {
        println!("{success_message}");
    } else {
        print_api_error(response).await?;
    }

    Ok(())
}

pub async fn run_command(cli: Cli) -> Result<()> {
    let client = Client::new();
    let base_url = cli.manager_addr;

    match cli.command {
        Command::Create { config, boot } => {
            let url = format!("{}/vms", base_url);
            let vm_config =
                serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&config)?)?;
            let body = DebugCreateVMRequest { vm_config, boot };
            let response = client.put(&url).json(&body).send().await?;

            print_message_response(response, "VM create request sent successfully").await?;
        }
    }

    Ok(())
}
