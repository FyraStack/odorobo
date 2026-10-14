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
        /// Storage URI: file://, rbd://, or iscsi:// (local files use file:///absolute/path).
        #[arg(long, env = "ODOROBO_VM_IMAGE")]
        image: String,
        /// Name to assign to the VM.
        #[arg(long, default_value = "test_vm")]
        name: String,
        /// Number of virtual CPUs to assign to the VM.
        #[arg(long, default_value_t = 4)]
        vcpus: u32,
        /// Memory to assign to the VM (for example, 4GiB).
        #[arg(long, default_value = "4GiB")]
        memory: ByteSize,
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
        Command::Create {
            image,
            name,
            vcpus,
            memory,
        } => {
            let vm = VmManifest {
                api_version: odorobo::manifest::MANIFEST_VERSION,
                id: Ulid::generate(),
                desired: DesiredState {
                    metadata: Metadata {
                        name,
                        ..Default::default()
                    },
                    compute: Compute {
                        vcpus,
                        memory_bytes: memory.as_u64(),
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

            vm.validate()?;
            let request = CreateVMRequest { vm };

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

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use bytesize::ByteSize;
    use clap::Parser;

    #[test]
    fn create_options_default_to_current_vm_settings() {
        let cli = Cli::try_parse_from(["odoroboctl", "create", "--image", "disk.img"])
            .expect("default create options should parse");

        match cli.command {
            Command::Create {
                name,
                vcpus,
                memory,
                ..
            } => {
                assert_eq!(name, "test_vm");
                assert_eq!(vcpus, 4);
                assert_eq!(memory, ByteSize::gib(4));
            }
            _ => panic!("expected create command"),
        }
    }

    #[tokio::test]
    async fn create_rejects_invalid_settings_before_sending_request() {
        let invalid_settings = [
            (
                " ",
                4,
                ByteSize::gib(4),
                "metadata must define a non-empty name",
            ),
            ("test_vm", 0, ByteSize::gib(4), "at least one vCPU"),
            (
                "test_vm",
                4,
                ByteSize::b(0),
                "memory must be greater than zero",
            ),
        ];

        for (name, vcpus, memory, expected_error) in invalid_settings {
            let error = super::run_command(Cli {
                manager_addr: "http://127.0.0.1:1".to_owned(),
                command: Command::Create {
                    image: "file:///var/lib/odorobo/disk.img".to_owned(),
                    name: name.to_owned(),
                    vcpus,
                    memory,
                },
            })
            .await
            .expect_err("invalid VM settings should fail locally");

            assert!(
                error.to_string().contains(expected_error),
                "unexpected error for invalid settings: {error}"
            );
        }
    }

    #[tokio::test]
    async fn create_sends_valid_settings_to_manager() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test manager");
        let manager_addr = listener.local_addr().expect("get test manager address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept request");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("read request line");
            assert!(line.starts_with("POST /vms "));

            let mut content_length = None;
            loop {
                line.clear();
                let bytes_read = reader.read_line(&mut line).await.expect("read header");
                assert_ne!(bytes_read, 0, "request ended before headers completed");
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length =
                            Some(value.trim().parse::<usize>().expect("valid content length"));
                    }
                }
            }

            let mut body = vec![0; content_length.expect("content-length header")];
            reader
                .read_exact(&mut body)
                .await
                .expect("read complete request body");
            let request: serde_json::Value =
                serde_json::from_slice(&body).expect("request body should be valid JSON");
            assert_eq!(request["vm"]["desired"]["metadata"]["name"], "web");
            assert_eq!(request["vm"]["desired"]["compute"]["vcpus"], 2);
            assert_eq!(
                request["vm"]["desired"]["compute"]["memory_bytes"],
                ByteSize::gib(8).as_u64()
            );

            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .expect("write response");
        });

        super::run_command(Cli {
            manager_addr: format!("http://{manager_addr}"),
            command: Command::Create {
                image: "file:///var/lib/odorobo/disk.img".to_owned(),
                name: "web".to_owned(),
                vcpus: 2,
                memory: ByteSize::gib(8),
            },
        })
        .await
        .expect("valid VM settings should be sent to the manager");
        server.await.expect("test manager should finish");
    }

    #[test]
    fn create_options_accept_vm_settings() {
        let cli = Cli::try_parse_from([
            "odoroboctl",
            "create",
            "--image",
            "disk.img",
            "--name",
            "web",
            "--vcpus",
            "8",
            "--memory",
            "16GiB",
        ])
        .expect("custom create options should parse");

        match cli.command {
            Command::Create {
                name,
                vcpus,
                memory,
                ..
            } => {
                assert_eq!(name, "web");
                assert_eq!(vcpus, 8);
                assert_eq!(memory, ByteSize::gib(16));
            }
            _ => panic!("expected create command"),
        }
    }
}
