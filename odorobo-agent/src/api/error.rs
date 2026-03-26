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
}
