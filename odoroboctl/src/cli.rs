use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reqwest::Client;
use stable_eyre::Result;

#[derive(Parser)]
#[command(
    name = "odoroboctl",
    about = "Command-line interface for odorobo agent"
)]
pub struct Cli {
    /// Address of the odorobo agent API server, e.g. "http://localhost:8890"
    #[arg(
        long,
        env = "ODOROBO_AGENT_ADDR",
        default_value = "http://localhost:8890"
    )]
    pub agent_addr: String,

    /// Directory for storing runtime files for odorobo agent, such as instance and VM state files.
    #[arg(
        long,
        env = "ODOROBO_AGENT_RUNTIME_DIR",
        default_value = "/run/odorobo"
    )]
    pub agent_runtime_dir: String,

    /// Path to the ch-remote binary, used for passing through with `odoroboctl ch-remote ...`
    #[arg(long, env = "CH_REMOTE_PATH", default_value = "ch-remote")]
    pub ch_remote_path: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List all VMs
    List,
    /// Get VM information
    Info { vmid: String },
    /// Ping VM (check if VMM is running)
    Ping { vmid: String },
    /// Spawn (create and start) a new VM
    Spawn {
        /// VM ID
        vmid: String,

        /// Path to the VM config file
        /// (in Cloud Hypervisor JSON format)
        config: Option<PathBuf>,

        /// Boot the VM after creation
        #[arg(long)]
        boot: bool,
    },

    /// Create a configuration for a new VM,
    /// optionally also booting it immediately after creation (if `--boot` is specified).
    Create {
        /// VM ID
        vmid: String,

        /// Path to the VM config file
        /// (in Cloud Hypervisor JSON format)
        config: PathBuf,

        /// Boot the VM after creation
        #[arg(long)]
        boot: bool,
    },
    /// Delete a VM configuration (without deleting the VM instance itself)
    Delete { vmid: String },
    /// Boot a VM
    Boot { vmid: String },
    /// Pause a running VM
    Pause { vmid: String },
    /// Resume a paused VM
    Resume { vmid: String },
    /// Shutdown a VM gracefully
    Shutdown { vmid: String },
    /// Send ACPI power button event to VM
    AcpiShutdown { vmid: String },
    /// Destroy (delete) a VM
    Destroy { vmid: String },
    /// Pass-through to Cloud Hypervisor CLI
    ChRemote {
        vmid: String,
        /// Any arbitrary args to pass through to ch-remote, e.g. `odoroboctl ch-remote myvm ping`
        #[arg(last = true)]
        args: Vec<String>,
    },
}

pub async fn run_command(cli: Cli) -> Result<()> {
    let client = Client::new();
    let base_url = format!("{}/vms", cli.agent_addr);

    match cli.command {
        Command::List => {
            let response = client.get(&base_url).send().await?;

            if response.status().is_success() {
                let vms: Vec<String> = response.json().await?;
                println!("VMs:");
                for vm in vms {
                    println!("- {}", vm);
                }
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Info { vmid } => {
            let url = format!("{}/{}", base_url, vmid);
            let response = client.get(&url).send().await?;

            if response.status().is_success() {
                println!("{}", response.text().await?);
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Ping { vmid } => {
            let url = format!("{}/{}/ping", base_url, vmid);
            let response = client.get(&url).send().await?;

            if response.status().is_success() {
                println!("{}", response.text().await?);
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Spawn { vmid, config, boot } => {
            let url = format!("{base_url}/{vmid}?boot={boot}");

            let config_content = if let Some(config_path) = config {
                Some(std::fs::read_to_string(&config_path)?)
            } else {
                None
            };

            let response = client
                .put(&url)
                .header("Content-Type", "application/json")
                .body(config_content.unwrap_or_default())
                .send()
                .await?;

            if response.status().is_success() {
                println!("{}", response.text().await?);
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Create { vmid, config, boot } => {
            let url = format!("{base_url}/{vmid}/config?boot={boot}");
            let config_content = std::fs::read_to_string(&config)?;
            let response = client
                .put(&url)
                .header("Content-Type", "application/json")
                .body(config_content)
                .send()
                .await?;

            if response.status().is_success() {
                println!("VM created successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Delete { vmid } => {
            let url = format!("{base_url}/{vmid}/config");
            let response = client.delete(&url).send().await?;

            if response.status().is_success() {
                println!("VM configuration deleted successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Boot { vmid } => {
            let url = format!("{}/{}/boot", base_url, vmid);
            let response = client.put(&url).send().await?;

            if response.status().is_success() {
                println!("VM booted successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Pause { vmid } => {
            let url = format!("{}/{}/pause", base_url, vmid);
            let response = client.put(&url).send().await?;

            if response.status().is_success() {
                println!("VM paused successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Resume { vmid } => {
            let url = format!("{}/{}/resume", base_url, vmid);
            let response = client.put(&url).send().await?;

            if response.status().is_success() {
                println!("VM resumed successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Shutdown { vmid } => {
            let url = format!("{}/{}/shutdown", base_url, vmid);
            let response = client.put(&url).send().await?;

            if response.status().is_success() {
                println!("VM shutdown initiated");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::AcpiShutdown { vmid } => {
            let url = format!("{}/{}/acpi_shutdown", base_url, vmid);
            let response = client.put(&url).send().await?;

            if response.status().is_success() {
                println!("ACPI shutdown signal sent");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::Destroy { vmid } => {
            let url = format!("{}/{}", base_url, vmid);
            let response = client.delete(&url).send().await?;

            if response.status().is_success() {
                println!("VM destroyed successfully");
            } else {
                eprintln!("Error: {:?}", response.text().await?);
            }
        }

        Command::ChRemote { vmid, args } => {
            let runtime_dir = cli.agent_runtime_dir;
            let socket_id = format!("{runtime_dir}/vms/{vmid}/ch.sock");
            let mut child = std::process::Command::new(&cli.ch_remote_path)
                .arg("--api-socket")
                .arg(socket_id)
                .args(args)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()?;

            let status = child.wait()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    Ok(())
}
