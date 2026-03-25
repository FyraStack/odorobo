//! Temporary state management for the agent.
//! in-memory state inside /run is not persisted across reboots, so we can use it to store runtime state of the agent and VMs like currently running
//! VMs, and their instances, etc.
//!
//! Persistent stuff should be stored in the database to reconcile from

use stable_eyre::Result;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const RUNTIME_VMS_DIR: &str = "/run/odorobo/vms";

// do some kind of API here, call home to gateway/orchestrator
// then create systemd services of `odorobo-ch@<vmid>.service` for each VM

pub struct VMInstance {
    pub id: String, // ulid
    pub ch_socket_path: PathBuf,
}

impl VMInstance {
    pub fn info() -> Result<Self> {
        // call info from the socket path
        // ch-remote --api-socket {self.ch_socket_path} info
        todo!()
    }

    /// Call a custom API path with an optional body on the socket
    pub fn call(path: &str, body: Option<&str>) -> Result<()> {
        // call the api socket with the path and value
        // ch-remote --api-socket {self.ch_socket_path} call {path} {value}
        todo!()
    }
}

pub fn init() -> Result<()> {
    if !Path::new(RUNTIME_VMS_DIR).exists() {
        fs::create_dir_all(RUNTIME_VMS_DIR)?;
    }
    Ok(())
}

pub fn list_vms() -> Result<Vec<String>> {
    Ok(fs::read_dir(RUNTIME_VMS_DIR)?
        .filter_map(|entry| {
            entry.ok().and_then(|e| {
                if e.file_type().ok()?.is_dir() {
                    Some(e.file_name().to_string_lossy().to_string())
                } else {
                    None
                }
            })
        })
        .collect::<Vec<String>>())
}
