use thiserror::Error;

use subvol_core::types::StorageError;

pub type NbdResult<T> = Result<T, NbdError>;

#[derive(Debug, Error)]
pub enum NbdError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("client disconnected")]
    Disconnected,

    #[error("unknown export: {0}")]
    UnknownExport(String),

    #[error("protocol error: {0}")]
    Protocol(&'static str),
}
