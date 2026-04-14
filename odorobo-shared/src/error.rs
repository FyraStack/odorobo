use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OdoroboError {
    #[error("{0}")]
    Report(#[from] stable_eyre::Report),
}
