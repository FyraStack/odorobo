use bytesize::ByteSize;
use clap::{Parser, Subcommand};
use odorobo::{
    manifest::{Boot, Compute, DesiredState, Metadata, Storage, VmManifest},
    types::CreateVMRequest,
};
use reqwest::{Client, Response};
use serde::Deserialize;
use stable_eyre::Result;
use ulid::Ulid;

#[derive(Parser)]
#[command(
    name = "odoroboctl",
    about = "Command-line interface for odorobo manager"
)]
pub struct Cli {
    /// Address of the odorobo manager scheduler API server, e.g. "<http://localhost:3000>"
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
    /// Create a VM via the scheduler endpoint.
    Create {
        /// Path or URI of the VM disk image.
        #[arg(long, env = "ODOROBO_VM_IMAGE")]
        image: String,
    },

    /// List VMs currently known by the manager/agent.
    List,

    /// Delete a VM by ID.
    Delete {
        /// VM ID in ULID format
        vmid: String,
    },

    /// Shut down a VM by ID.
    Shutdown {
        /// VM ID in ULID format
        vmid: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct VmId(String);

#[derive(Debug, Deserialize)]
struct VMListResponse {
    vms: Vec<VmId>,
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
        let text = response.text().await?;
        println!("{text}");
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
        Command::Create { image } => {
            let vm = VmManifest {
                api_version: odorobo::manifest::MANIFEST_VERSION,
                id: Ulid::generate(),
                desired: DesiredState {
                    metadata: Metadata {
                        name: "test_vm".to_owned(),
                        ..Default::default()
                    },
                    compute: Compute {
                        vcpus: 4,
                        memory: ByteSize::gib(4),
                        ..Default::default()
                    },
                    storage: vec![Storage {
                        id: "root".to_owned(),
                        uri: Some(image),
                        ..Default::default()
                    }],
                    boot: Boot {
                        start: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                observed: None,
            };

            let request = CreateVMRequest { vm, boot: true };

            let url = format!("{base_url}/vms");
            let response = client.post(&url).json(&request).send().await?;

            println!("{:?}", response.url());

            print_message_response(response, "VM create request sent successfully").await?;
        }
        Command::List => {
            let url = format!("{base_url}/vms");
            let response = client.get(&url).send().await?;

            if response.status().is_success() {
                let body = response.json::<VMListResponse>().await?;
                for vm in body.vms {
                    println!("{}", vm.0);
                }
            } else {
                print_api_error(response).await?;
            }
        }
        Command::Delete { vmid } => {
            let url = format!("{base_url}/vms/{vmid}");
            let response = client.delete(&url).send().await?;

            print_message_response(response, "VM delete request sent successfully").await?;
        }
        Command::Shutdown { vmid } => {
            let url = format!("{base_url}/vms/{vmid}/shutdown");
            let response = client.put(&url).send().await?;

            print_message_response(response, "VM shutdown request sent successfully").await?;
        }
    }

    Ok(())
}
