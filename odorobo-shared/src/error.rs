use thiserror::Error;

#[derive(Error, Debug)]
pub enum OdoroboError {
    #[error("")]
    Foobar,
}
