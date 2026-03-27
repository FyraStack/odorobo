use axum_responses::HttpError;
use thiserror::Error;

#[derive(Debug, Error, HttpError)]
pub enum ApiError {
    #[error("VM not found: {0}")]
    #[http(code = 404)]
    VmNotFound(String),

    #[error("Failed to get VM info")]
    #[http(code = 500)]
    VmInfoFailed,

    #[error("Failed to list VMs")]
    #[http(code = 500)]
    ListFailed,

    #[error("Failed to create VM")]
    #[http(code = 500)]
    CreateFailed,

    #[error("Failed to open VM console")]
    #[http(code = 500)]
    ConsoleFailed,

    #[error("Failed to proxy Cloud Hypervisor API request")]
    #[http(code = 500)]
    PassthroughFailed,
}
