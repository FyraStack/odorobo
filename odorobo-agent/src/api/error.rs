use axum_responses::{thiserror::Error, HttpError};
use stable_eyre::Report;

use crate::state::ChApiError;

#[derive(Debug, Error, HttpError)]
pub enum ApiError {
    #[error("Invalid VM ID: {msg}")]
    #[http(code = 400, message = msg)]
    InvalidVmId { msg: String },

    #[error("VM not found: {vmid}")]
    #[http(code = 404, message = vmid)]
    VmNotFound { vmid: String },

    #[error("Failed to get VM info: {msg}")]
    #[http(code = 500, message = msg, errors = errors)]
    VmInfoFailed { msg: String, errors: Vec<String> },

    #[error("Failed to list VMs: {msg}")]
    #[http(code = 500, message = msg)]
    ListFailed { msg: String },

    #[error("Failed to create VM: {msg}")]
    #[http(code = 500, message = msg, errors = errors)]
    CreateFailed { msg: String, errors: Vec<String> },

    #[error("Failed to open VM console: {msg}")]
    #[http(code = 500, message = msg)]
    ConsoleFailed { msg: String },

    #[error("Failed to proxy Cloud Hypervisor API request: {msg}")]
    #[http(code = 500, message = msg)]
    PassthroughFailed { msg: String },

    #[error("Cloud Hypervisor API error: {msg}")]
    #[http(code = 502, message = msg, errors = errors)]
    ChApiFailed { msg: String, errors: Vec<String> },

    #[error("Failed to delete VM configuration: configuration: {msg}")]
    #[http(code = 500, message = msg, errors = errors)]
    DeleteConfigFailed { msg: String, errors: Vec<String> },

    #[error("Failed to delete VM configuration: configuration: {msg}")]
    #[http(code = 500, message = msg, errors = errors)]
    CreateConfigFailed { msg: String, errors: Vec<String> },
}

impl ApiError {
    fn find_ch_api_error(error: &Report) -> Option<&ChApiError> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ChApiError>())
    }

    fn from_report(error: Report, fallback: impl FnOnce(String, Vec<String>) -> Self) -> Self {
        if let Some(ch_error) = Self::find_ch_api_error(&error) {
            return match ch_error {
                ChApiError::Api { status, errors } => fallback(
                    format!("Cloud Hypervisor API returned status {}", status.as_u16()),
                    errors.clone(),
                ),
                ChApiError::Client(_) => fallback(format!("{error:?}"), vec![]),
            };
        }

        fallback(format!("{error:?}"), vec![])
    }

    pub fn vm_info(error: Report) -> Self {
        Self::from_report(error, |msg, errors| Self::VmInfoFailed { msg, errors })
    }

    pub fn create(error: Report) -> Self {
        Self::from_report(error, |msg, errors| Self::CreateFailed { msg, errors })
    }

    pub fn create_config(error: Report) -> Self {
        Self::from_report(error, |msg, errors| Self::CreateConfigFailed {
            msg,
            errors,
        })
    }

    pub fn delete_config(error: Report) -> Self {
        Self::from_report(error, |msg, errors| Self::DeleteConfigFailed {
            msg,
            errors,
        })
    }
}
