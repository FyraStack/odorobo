use axum_responses::{thiserror::Error, HttpError};

#[derive(Debug, Error, HttpError)]
pub enum ApiError {
    #[error("Invalid VM ID: {msg}")]
    #[http(code = 400, message = msg)]
    InvalidVmId { msg: String },

    #[error("VM not found: {vmid}")]
    #[http(code = 404, message = vmid)]
    VmNotFound { vmid: String },

    #[error("Failed to get VM info: {msg}")]
    #[http(code = 500, message = msg)]
    VmInfoFailed { msg: String },

    #[error("Failed to list VMs: {msg}")]
    #[http(code = 500, message = msg)]
    ListFailed { msg: String },

    #[error("Failed to create VM: {msg}")]
    #[http(code = 500, message = msg)]
    CreateFailed { msg: String },

    #[error("Failed to open VM console: {msg}")]
    #[http(code = 500, message = msg)]
    ConsoleFailed { msg: String },

    #[error("Failed to proxy Cloud Hypervisor API request: {msg}")]
    #[http(code = 500, message = msg)]
    PassthroughFailed { msg: String },

    #[error("Failed to delete VM configuration: configuration: {msg}")]
    #[http(code = 500, message = msg)]
    DeleteConfigFailed { msg: String },

    #[error("Failed to delete VM configuration: configuration: {msg}")]
    #[http(code = 500, message = msg)]
    CreateConfigFailed { msg: String },
}
