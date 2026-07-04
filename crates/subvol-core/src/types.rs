use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watermark {
    Stale = 0,
    Low = 1,
    Medium = 2,
    High = 3,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("no memory")]
    NoMem,
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("COW needed: {0}")]
    CowNeeded(String),
    #[error("btree node full — need rewrite or split")]
    BtreeNodeFull,
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e.to_string())
    }
}

pub type BgTaskHandle = Arc<tokio::task::JoinHandle<()>>;
